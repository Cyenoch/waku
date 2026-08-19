use super::*;

fn workspace_ack(
    workspace: &wakuwaku_client::WorkspaceClient,
    operation: wakuwaku_client::WorkspaceOperation,
) -> anyhow::Result<()> {
    match workspace.request(operation)? {
        wakuwaku_client::WorkspaceResult::Ack => Ok(()),
        _ => anyhow::bail!("the daemon returned an invalid workspace response"),
    }
}

fn start_driver(mut request: DriverStartRequest, cwd: PathBuf) -> anyhow::Result<PreparedDriver> {
    request.options.cwd = cwd;
    let (event_tx, events) = driver::event_channel(request.event_wake);
    let handle = driver::start_remote(
        request.daemon_client,
        request.session_id,
        request.options,
        Some(request.task),
        event_tx,
    )?;
    Ok(PreparedDriver { handle, events })
}

fn attach_driver(
    daemon: wakuwaku_client::DaemonSupervisor,
    session_id: Uuid,
    event_wake: smol::channel::Sender<()>,
) -> anyhow::Result<Option<(AgentSession, PreparedDriver)>> {
    let Some(session) = wakuwaku_client::persistence::hydrate_session(&daemon, session_id)? else {
        return Ok(None);
    };
    let response = daemon.client().request(
        session_id,
        Uuid::nil(),
        wakuwaku_client::Command::AttachSession,
    )?;
    let wakuwaku_client::ResponsePayload::SessionRuntime {
        runtime_id,
        supports_steer,
    } = response
    else {
        anyhow::bail!("WakuWaku daemon returned an invalid runtime attachment response");
    };
    let Some(runtime_id) = runtime_id else {
        return Ok(None);
    };
    let (event_tx, events) = driver::event_channel(event_wake);
    let handle = driver::attach_remote(
        daemon.client(),
        session_id,
        runtime_id,
        supports_steer,
        session.runtime_event_cursor,
        event_tx,
    )?;
    Ok(Some((session, PreparedDriver { handle, events })))
}

fn load_remote_task_state(
    client: &wakuwaku_client::DaemonClient,
) -> anyhow::Result<RemoteTaskStateSnapshot> {
    let response = client.request(
        Uuid::nil(),
        Uuid::nil(),
        wakuwaku_client::Command::LoadTaskState,
    )?;
    let wakuwaku_client::ResponsePayload::TaskState {
        projects,
        mut sessions,
        ..
    } = response
    else {
        anyhow::bail!("WakuWaku daemon returned an invalid task-state response");
    };
    for session in &mut sessions {
        session.detail_loaded = false;
    }
    Ok(RemoteTaskStateSnapshot { projects, sessions })
}

/// Merge the daemon's list-only session projection into the desktop catalog.
///
/// Existing rows may already contain a hydrated transcript, so only list
/// metadata is copied from the projection. A locally attached runtime remains
/// authoritative for transient status and timestamps until its own events are
/// drained.
pub(super) fn merge_remote_session_catalog(
    local: &mut Vec<AgentSession>,
    remote: Vec<AgentSession>,
    has_local_runtime: impl Fn(Uuid) -> bool,
) -> Vec<Uuid> {
    let remote_ids = remote
        .iter()
        .map(|session| session.id)
        .collect::<HashSet<_>>();
    let removed = local
        .iter()
        .filter(|session| session.has_started() && !remote_ids.contains(&session.id))
        .map(|session| session.id)
        .collect::<Vec<_>>();
    local.retain(|session| !session.has_started() || remote_ids.contains(&session.id));

    for remote in remote {
        if let Some(local) = local.iter_mut().find(|session| session.id == remote.id) {
            local.title = remote.title;
            local.auto_title = remote.auto_title;
            local.project_id = remote.project_id;
            local.provider = remote.provider;
            local.model = remote.model;
            local.created_at = remote.created_at;
            local.last_reply_at = remote.last_reply_at;
            if !has_local_runtime(local.id) {
                local.status = remote.status;
                local.updated_at = remote.updated_at;
            }
        } else {
            local.push(remote);
        }
    }

    removed
}

/// Perform every blocking operation between accepting a submission and
/// starting its provider. This function is called only from the background
/// executor; the UI thread owns applying the returned workspace afterward.
fn prepare_submission(
    workspace_client: wakuwaku_client::WorkspaceClient,
    project: Project,
    workspace: SessionWorkspace,
    driver_start: Option<anyhow::Result<DriverStartRequest>>,
    session_id: Uuid,
    prompt: &str,
    turn_count: usize,
) -> anyhow::Result<PreparedSubmission> {
    let workspace = match workspace {
        SessionWorkspace::NewWorktree { base_branch } => {
            if project.is_projectless() {
                anyhow::bail!("a projectless task cannot create a Git worktree");
            }
            let created = match workspace_client.request(
                wakuwaku_client::WorkspaceOperation::CreateWorktree {
                    project_path: project.path.clone(),
                    project_id: project.id,
                    session_id,
                    prompt: prompt.to_owned(),
                    base_branch,
                },
            )? {
                wakuwaku_client::WorkspaceResult::WorktreeCreated { worktree } => worktree,
                _ => anyhow::bail!("the daemon returned an invalid worktree response"),
            };
            SessionWorkspace::Worktree {
                path: created.path,
                branch: created.branch,
            }
        }
        workspace => workspace,
    };
    let project_path = workspace.path().unwrap_or(&project.path);

    // Every turn gets its own immutable starting snapshot. Reusing the prior
    // response's ending ref would attribute branch switches or terminal edits
    // made between turns to the next response.
    let checkpoint_warning = workspace_ack(
        &workspace_client,
        wakuwaku_client::WorkspaceOperation::CaptureTurnStart {
            cwd: project_path.to_path_buf(),
            session_id,
            turn_count,
        },
    )
    .err()
    .map(|error| tr!("errors.capture_pre_turn_checkpoint", error = error));

    // Daemon Start talks HTTP and can block on auth overlay. Keep it behind
    // the same animated preparation boundary as Git work so the last spinner
    // frame does not freeze just before Stop appears.
    let driver = driver_start.map(|request| {
        request.and_then(|request| start_driver(request, project_path.to_path_buf()))
    });

    Ok(PreparedSubmission {
        workspace,
        checkpoint_warning,
        driver,
    })
}

/// Everything a past-message resend needs after the UI accepts it.
///
/// Rewind is daemon-owned. The daemon has the persisted transcript, Git
/// checkpoints, and the embedded provider runtime, so the desktop submits one
/// typed command and applies the returned session snapshot.
struct MessageRewindRequest {
    daemon_client: wakuwaku_client::DaemonClient,
    session_id: Uuid,
    turn_count: usize,
}

struct PreparedMessageRewind {
    session: AgentSession,
    cleanup_error: Option<String>,
}
struct MessageRewindCompletion {
    edit: MessageEdit,
    submission: ComposerSubmission,
    edited_message_id: Uuid,
    original_message: Message,
    previous_status: SessionStatus,
    result: Result<PreparedMessageRewind, String>,
}

fn perform_message_rewind(request: MessageRewindRequest) -> Result<PreparedMessageRewind, String> {
    let response = request
        .daemon_client
        .request(
            request.session_id,
            Uuid::nil(),
            wakuwaku_client::Command::RewindSessionToMessage {
                turn_count: request.turn_count,
            },
        )
        .map_err(|error| error.to_string())?;
    let wakuwaku_client::ResponsePayload::SessionRewound {
        session,
        cleanup_warning,
    } = response
    else {
        return Err("WakuWaku daemon returned an invalid rewind response".to_owned());
    };
    Ok(PreparedMessageRewind {
        session: *session,
        cleanup_error: cleanup_warning,
    })
}

/// Everything a response fork needs after the click has been accepted.
///
/// Forking is likewise a single daemon operation. Keeping this request free of
/// provider-specific cursors is important: a `ProviderId` identifies the
/// configured endpoint, while the daemon owns any runtime conversation state.
struct ResponseForkRequest {
    daemon_client: wakuwaku_client::DaemonClient,
    session_id: Uuid,
    turn_count: usize,
}

struct PreparedResponseFork {
    session: AgentSession,
    checkpoint_warning: Option<String>,
}

fn perform_response_fork(request: ResponseForkRequest) -> Result<PreparedResponseFork, String> {
    let response = request
        .daemon_client
        .request(
            request.session_id,
            Uuid::nil(),
            wakuwaku_client::Command::ForkSessionFromResponse {
                turn_count: request.turn_count,
            },
        )
        .map_err(|error| error.to_string())?;
    let wakuwaku_client::ResponsePayload::SessionForked {
        session,
        checkpoint_warning,
    } = response
    else {
        return Err("WakuWaku daemon returned an invalid fork response".to_owned());
    };
    Ok(PreparedResponseFork {
        session: *session,
        checkpoint_warning,
    })
}

impl Waku {
    pub(super) fn restart_task_state_sync(&self) {
        let clients = self.daemon.subscribe_clients();
        let results = self.task_state_sync_tx.clone();
        let event_wake = self.event_wake_tx.clone();
        std::thread::Builder::new()
            .name("wakuwaku-task-state-sync".into())
            .spawn(move || {
                let Ok(mut client) = clients.recv() else {
                    return;
                };
                loop {
                    while let Ok(newer) = clients.try_recv() {
                        client = newer;
                    }
                    let revisions = client.subscribe_task_state();
                    let result = load_remote_task_state(&client).map_err(|error| error.to_string());
                    if results.send(result).is_err() {
                        return;
                    }
                    signal_event_pump(&event_wake);
                    client = loop {
                        crossbeam_channel::select! {
                            recv(clients) -> replacement => {
                                let Ok(mut replacement) = replacement else {
                                    return;
                                };
                                while let Ok(newer) = clients.try_recv() {
                                    replacement = newer;
                                }
                                break replacement;
                            }
                            recv(revisions) -> revision => {
                                if revision.is_err() {
                                    // Managed replacement publishes the new
                                    // client after the old socket closes. Wait
                                    // for that publication instead of exiting
                                    // the task-state sync worker permanently.
                                    let Ok(replacement) = clients.recv() else {
                                        return;
                                    };
                                    break replacement;
                                }
                                while revisions.try_recv().is_ok() {}
                                let result = load_remote_task_state(&client)
                                    .map_err(|error| error.to_string());
                                if results.send(result).is_err() {
                                    return;
                                }
                                signal_event_pump(&event_wake);
                            }
                        }
                    };
                }
            })
            .ok();
    }

    fn drain_task_state_sync_events(&mut self, cx: &mut Context<Self>) -> bool {
        let mut latest = None;
        while let Ok(result) = self.task_state_sync_events.try_recv() {
            latest = Some(result);
        }
        let Some(result) = latest else {
            return false;
        };
        match result {
            Ok(snapshot) => {
                self.apply_remote_task_state(snapshot, cx);
                true
            }
            Err(error) => {
                eprintln!("could not refresh daemon task state: {error}");
                false
            }
        }
    }

    fn apply_remote_task_state(
        &mut self,
        snapshot: RemoteTaskStateSnapshot,
        cx: &mut Context<Self>,
    ) {
        if self.auth_statuses.is_empty() {
            self.refresh_provider_auth_statuses(cx);
        }
        let runtime_ids = self.runtimes.keys().copied().collect::<HashSet<_>>();
        let removed = merge_remote_session_catalog(
            &mut self.state.sessions,
            snapshot.sessions,
            |session_id| runtime_ids.contains(&session_id),
        );
        for session_id in &removed {
            self.runtime_attach_pending.remove(session_id);
            self.runtime_attach_misses.remove(session_id);
            self.runtimes.remove(session_id);
            self.background_work.remove(session_id);
            self.remove_right_panel_session_state(*session_id);
        }
        self.state.projects = snapshot.projects;

        let attach = self
            .state
            .sessions
            .iter()
            .filter(|session| {
                session.status.is_busy()
                    || (self.state.selected_session == Some(session.id) && session.has_started())
            })
            .map(|session| session.id)
            .collect::<Vec<_>>();
        for session_id in attach {
            self.start_runtime_attachment(session_id, cx);
        }

        if self.state.selected_session.is_some_and(|selected| {
            !self
                .state
                .sessions
                .iter()
                .any(|session| session.id == selected)
        }) {
            let previous_project = self.state.selected_project;
            self.state.selected_session = None;
            let next = self
                .state
                .sessions
                .iter()
                .filter(|session| {
                    previous_project.is_none_or(|project| session.project_id == project)
                })
                .max_by_key(|session| session.updated_at)
                .map(|session| session.id)
                .or_else(|| {
                    self.state
                        .sessions
                        .iter()
                        .max_by_key(|session| session.updated_at)
                        .map(|session| session.id)
                });
            if let Some(next) = next {
                self.select_session(next, cx);
            } else if let Some(project_id) = self
                .state
                .selected_project
                .filter(|project_id| {
                    self.state
                        .projects
                        .iter()
                        .any(|project| project.id == *project_id)
                })
                .or_else(|| self.state.projects.first().map(|project| project.id))
            {
                self.state.selected_project = Some(project_id);
                let provider = self.state.last_provider.clone();
                self.create_session_for(project_id, provider, cx);
            }
        }
    }

    pub(super) fn start_runtime_attachment(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if self.runtimes.contains_key(&session_id)
            || !self.runtime_attach_pending.insert(session_id)
        {
            return;
        }
        let daemon = self.daemon.clone();
        let event_wake = self.event_wake_tx.clone();
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { attach_driver(daemon, session_id, event_wake) })
                .await;
            let _ = waku.update(cx, move |waku, cx| {
                waku.finish_runtime_attachment(session_id, result, cx);
            });
        })
        .detach();
    }

    fn finish_runtime_attachment(
        &mut self,
        session_id: Uuid,
        result: anyhow::Result<Option<(AgentSession, PreparedDriver)>>,
        cx: &mut Context<Self>,
    ) {
        if !self.runtime_attach_pending.remove(&session_id) {
            return;
        }
        match result {
            Ok(Some((session, prepared))) => {
                self.runtime_attach_misses.remove(&session_id);
                let Some(index) = self
                    .state
                    .sessions
                    .iter()
                    .position(|candidate| candidate.id == session_id)
                else {
                    return;
                };
                if !self.runtimes.contains_key(&session_id) {
                    self.state.sessions[index] = session;
                    self.install_prepared_driver(session_id, prepared, cx);
                    if self.state.selected_session == Some(session_id) {
                        self.reset_visible_state();
                        self.reset_transcript_rows(self.transcript_row_count());
                    }
                    cx.notify();
                }
            }
            Ok(None) => {
                let busy = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .is_some_and(|session| session.status.is_busy());
                if !busy {
                    self.runtime_attach_misses.remove(&session_id);
                    return;
                }
                let misses = self.runtime_attach_misses.entry(session_id).or_default();
                *misses = misses.saturating_add(1);
                if *misses < 4 {
                    cx.spawn(async move |waku, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(250))
                            .await;
                        let _ = waku.update(cx, |waku, cx| {
                            waku.start_runtime_attachment(session_id, cx);
                        });
                    })
                    .detach();
                } else {
                    self.runtime_attach_misses.remove(&session_id);
                    self.interrupt_orphaned_runtime(session_id, cx);
                }
            }
            Err(error) => {
                eprintln!("could not attach desktop to daemon session {session_id}: {error:#}");
            }
        }
    }

    fn interrupt_orphaned_runtime(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let project_paths = self
            .state
            .projects
            .iter()
            .map(|project| (project.id, project.path.clone()))
            .collect::<HashMap<_, _>>();
        let mut checkpoint = None;
        if let Some(session) = self.state.session_mut(session_id) {
            if !session.status.is_busy() {
                return;
            }
            session.status = SessionStatus::Idle;
            let interrupted_turn_count = session
                .turns
                .last_mut()
                .filter(|turn| turn.status == TurnStatus::Running)
                .map(|turn| {
                    turn.status = TurnStatus::Interrupted;
                    turn.completed_at = Some(unix_time());
                    turn.turn_count
                });
            if let Some(turn_count) = interrupted_turn_count {
                let project_path = session
                    .workspace
                    .path()
                    .map(Path::to_path_buf)
                    .or_else(|| project_paths.get(&session.project_id).cloned());
                checkpoint = project_path.map(|project_path| PendingCheckpointCapture {
                    session_id,
                    turn_count,
                    project_path,
                });
            }
            for message in &mut session.messages {
                message.streaming = false;
            }
            for block in &mut session.transcript_blocks {
                block.activities.retain(|activity| {
                    activity
                        .reasoning
                        .as_ref()
                        .is_none_or(|reasoning| !reasoning.content.trim().is_empty())
                });
                for activity in &mut block.activities {
                    activity.complete = true;
                }
            }
            session
                .transcript_blocks
                .retain(|block| !block.activities.is_empty());
        }
        if let Some(checkpoint) = checkpoint {
            self.pending_checkpoint_captures.push(checkpoint);
            self.start_pending_checkpoint_captures(cx);
        }
        if self.state.selected_session == Some(session_id) {
            self.reset_visible_state();
            self.reset_transcript_rows(self.transcript_row_count());
        }
        self.save();
        cx.notify();
    }

    pub fn composer_focus(&self, cx: &App) -> FocusHandle {
        self.composer.read(cx).focus()
    }

    pub(super) fn selected_project(&self) -> Option<&Project> {
        let id = self.state.selected_project?;
        self.state.projects.iter().find(|project| project.id == id)
    }

    pub(super) fn selected_session(&self) -> Option<&AgentSession> {
        let id = self.state.selected_session?;
        self.state.sessions.iter().find(|session| session.id == id)
    }

    fn active_turn_finished_event(
        &self,
        session_id: Uuid,
        outcome: crate::analytics::TurnOutcome,
    ) -> Option<crate::analytics::Event> {
        let session = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)?;
        let turn = session
            .turns
            .last()
            .filter(|turn| turn.status == TurnStatus::Running)?;
        Some(crate::analytics::Event::TurnFinished {
            provider: session.provider.as_str().to_owned(),
            turn_number: turn.turn_count,
            outcome,
            duration_seconds: unix_time().saturating_sub(turn.started_at),
        })
    }

    /// Completes a persisted turn and emits its anonymous outcome exactly
    /// once. All production turn-settlement paths go through this seam.
    pub(super) fn finish_active_turn_with_analytics(
        &mut self,
        session_id: Uuid,
        status: TurnStatus,
        outcome: crate::analytics::TurnOutcome,
    ) -> Option<(Uuid, usize)> {
        let event = self.active_turn_finished_event(session_id, outcome);
        let result = self
            .state
            .session_mut(session_id)?
            .finish_active_turn(status);
        if result.is_some()
            && let Some(event) = event
        {
            self.analytics.track(event);
        }
        result
    }

    /// Records a failed submission that is about to be unwound and therefore
    /// will not remain as a persisted turn.
    fn track_active_turn_outcome(&self, session_id: Uuid, outcome: crate::analytics::TurnOutcome) {
        if let Some(event) = self.active_turn_finished_event(session_id, outcome) {
            self.analytics.track(event);
        }
    }

    /// The directory every filesystem and provider operation for `session`
    /// must use. A not-yet-materialized worktree draft deliberately reads the
    /// local checkout until its first submission creates the isolated copy.
    pub(super) fn workspace_path_for_session<'a>(
        &'a self,
        session: &'a AgentSession,
    ) -> Option<&'a std::path::Path> {
        let project = self
            .state
            .projects
            .iter()
            .find(|project| project.id == session.project_id)?;
        Some(session.workspace.path().unwrap_or(&project.path))
    }

    pub(super) fn selected_workspace_path(&self) -> Option<&std::path::Path> {
        let session = self.selected_session()?;
        self.workspace_path_for_session(session)
    }

    /// Marks the session for the next save; see `PersistedState::session_mut`.
    pub(super) fn selected_session_mut(&mut self) -> Option<&mut AgentSession> {
        let id = self.state.selected_session?;
        self.state.session_mut(id)
    }

    pub(super) fn selected_runtime(&self) -> Option<&SessionRuntime> {
        self.runtimes.get(&self.state.selected_session?)
    }

    pub(super) fn configured_provider(&self, provider: &ProviderId) -> Option<&ExternalProvider> {
        self.state
            .external_providers
            .iter()
            .find(|candidate| &candidate.id == provider)
    }

    pub(super) fn model_for_session<'a>(&'a self, session: &'a AgentSession) -> Option<&'a str> {
        let model = session
            .model
            .as_deref()
            .filter(|value| !value.trim().is_empty())?;
        super::composer::catalog_supports_model(self.model_catalogs.get(&session.provider), model)
            .then_some(model)
    }

    pub(super) fn model_display_name(&self, provider: &ProviderId, model: Option<&str>) -> String {
        let endpoint = self
            .configured_provider(provider)
            .map(|provider| provider.name.as_str())
            .or_else(|| {
                wakuwaku_client::ProviderPreset::parse_id(provider.as_str())
                    .map(wakuwaku_client::ProviderPreset::display_name)
            })
            .unwrap_or_else(|| provider.as_str());
        match model {
            Some(model) if !model.trim().is_empty() => format!("{endpoint} · {model}"),
            _ => endpoint.to_owned(),
        }
    }

    pub(super) fn selected_transcript_blocks(&self) -> &[TranscriptBlock] {
        self.selected_session()
            .map(|session| session.transcript_blocks.as_slice())
            .unwrap_or(&[])
    }

    pub(super) fn save(&mut self) {
        self.last_stream_save = Instant::now();
        let daemon_error = self
            .daemon
            .update_settings(self.state.daemon_settings())
            .err()
            .map(|error| error.to_string());
        let app_error = self
            .store
            .save(&mut self.state)
            .err()
            .map(|error| error.to_string());
        if let Some(error) = daemon_error.or(app_error) {
            self.show_toast(tr!("errors.save_local_state", error = error));
        } else {
            self.stream_state_dirty = false;
        }
    }

    fn checkpoint_capture_pending(&self, session_id: Uuid, turn_count: usize) -> bool {
        self.checkpoint_captures_in_flight
            .contains(&(session_id, turn_count))
            || self
                .pending_checkpoint_captures
                .iter()
                .any(|capture| capture.session_id == session_id && capture.turn_count == turn_count)
    }

    fn ending_checkpoint_pending(&self, session_id: Uuid) -> bool {
        self.state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| session.turns.last())
            .filter(|turn| turn.status != TurnStatus::Running)
            .is_some_and(|turn| self.checkpoint_capture_pending(session_id, turn.turn_count))
    }

    fn defer_queue_drain(&mut self, session_id: Uuid) {
        if !self.pending_queue_drains.contains(&session_id) {
            self.pending_queue_drains.push(session_id);
        }
    }

    /// Queues the newest finished turn's checkpoint for capture.
    ///
    /// Bookkeeping only. The capture itself is upwards of ten `git`
    /// invocations, one of them a `git add -A` over the whole worktree, and the
    /// hottest caller is the driver-event drain that shares the UI thread with
    /// rendering — so the work belongs to
    /// [`Self::start_pending_checkpoint_captures`], which every caller that
    /// holds a `Context` runs straight after queueing.
    pub(super) fn capture_latest_turn_checkpoint_for(&mut self, session_id: Uuid) {
        let Some((session, turn_count)) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| {
                session
                    .turns
                    .last()
                    .filter(|turn| turn.status != TurnStatus::Running)
                    .map(|turn| (session, turn.turn_count))
            })
        else {
            return;
        };
        if self.checkpoint_capture_pending(session_id, turn_count) {
            return;
        }
        let Some(project_path) = self
            .workspace_path_for_session(session)
            .map(std::path::Path::to_path_buf)
        else {
            return;
        };
        self.pending_checkpoint_captures
            .push(PendingCheckpointCapture {
                session_id,
                turn_count,
                project_path,
            });
    }

    /// Runs queued turn checkpoints on the background executor.
    ///
    /// A capture lands a frame or many later, and the turn it belongs to may be
    /// gone by then, so the result is matched back by turn count rather than
    /// position. Nothing on screen waits for it: the transcript's rewind
    /// affordance appears when `invalidate_checkpoint_refs` prompts the next
    /// prefetch to notice the new ref.
    pub(super) fn start_pending_checkpoint_captures(&mut self, cx: &mut Context<Self>) {
        for request in std::mem::take(&mut self.pending_checkpoint_captures) {
            let PendingCheckpointCapture {
                session_id,
                turn_count,
                project_path,
            } = request;
            if !self
                .checkpoint_captures_in_flight
                .insert((session_id, turn_count))
            {
                continue;
            }
            let workspace = wakuwaku_client::WorkspaceClient::new(self.daemon.client());
            cx.spawn(async move |waku, cx| {
                let captured = cx
                    .background_executor()
                    .spawn({
                        let project_path = project_path.clone();
                        async move {
                            match workspace.request(
                                wakuwaku_client::WorkspaceOperation::CaptureTurn {
                                    cwd: project_path,
                                    session_id,
                                    turn_count,
                                },
                            )? {
                                wakuwaku_client::WorkspaceResult::Checkpoint { checkpoint } => {
                                    Ok(checkpoint)
                                }
                                _ => anyhow::bail!(
                                    "the daemon returned an invalid checkpoint response"
                                ),
                            }
                        }
                    })
                    .await;
                waku.update(cx, |waku, cx| {
                    waku.checkpoint_captures_in_flight
                        .remove(&(session_id, turn_count));
                    let selected = waku.state.selected_session == Some(session_id);
                    if selected {
                        waku.sync_transcript_rows();
                    }
                    let previous_kinds = if selected {
                        waku.transcript_row_kinds.borrow().clone()
                    } else {
                        Vec::new()
                    };
                    let checkpoint = match captured {
                        Ok(checkpoint) => checkpoint,
                        Err(error) => {
                            waku.show_toast(tr!("errors.capture_turn_checkpoint", error = error));
                            Checkpoint {
                                turn_count,
                                git_ref: checkpoint::checkpoint_ref(session_id, turn_count),
                                status: CheckpointStatus::Error,
                                files: Vec::new(),
                                additions: 0,
                                deletions: 0,
                                created_at: unix_time(),
                            }
                        }
                    };
                    waku.invalidate_checkpoint_refs();
                    let mut attached_turn_id = None;
                    if let Some(session) = waku.state.session_mut(session_id)
                        && let Some(turn) = session
                            .turns
                            .iter_mut()
                            .find(|turn| turn.turn_count == turn_count)
                    {
                        turn.checkpoint = Some(checkpoint);
                        attached_turn_id = Some(turn.id);
                    }
                    if let Some(turn_id) = attached_turn_id
                        && selected
                    {
                        // Reconcile a standalone card by row identity, then
                        // remeasure the terminal response when the card is
                        // hosted inline before its footer.
                        waku.splice_transcript_rows_after_visibility_change(&previous_kinds);
                        waku.remeasure_changed_files(turn_id);
                    }
                    let resume_queue = waku.pending_queue_drains.contains(&session_id);
                    if resume_queue {
                        waku.pending_queue_drains.retain(|id| *id != session_id);
                        waku.drain_queued_message(session_id, cx);
                    }
                    cx.notify();
                    if attached_turn_id.is_some() {
                        // Let the new transcript row paint before SQLite work.
                        // Without this save, a checkpoint that lands after the
                        // turn's final stream save can disappear on relaunch.
                        cx.spawn(async move |waku, cx| {
                            cx.background_executor().timer(STREAM_FRAME_INTERVAL).await;
                            let _ = waku.update(cx, |waku, _| waku.save());
                        })
                        .detach();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    pub(super) fn fork_session_from_response(
        &mut self,
        session_id: Uuid,
        turn_count: usize,
        cx: &mut Context<Self>,
    ) {
        if self.response_fork_preparations.contains_key(&session_id)
            || self.submission_preparations.contains(&session_id)
        {
            self.show_toast(tr!("session.response_cannot_fork"));
            cx.notify();
            return;
        }
        let Some(source) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
        else {
            self.show_toast(tr!("session.response_unavailable"));
            cx.notify();
            return;
        };
        if self.state.selected_session != Some(session_id)
            || !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed)
            || source
                .turns
                .get(turn_count.saturating_sub(1))
                .is_none_or(|turn| turn.turn_count != turn_count)
        {
            self.show_toast(tr!("session.response_cannot_fork"));
            cx.notify();
            return;
        }
        let provider = source.provider.clone();
        let request = ResponseForkRequest {
            daemon_client: self.daemon.client(),
            session_id,
            turn_count,
        };
        self.response_fork_preparations
            .insert(session_id, turn_count);
        self.hide_toast();
        cx.notify();
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { perform_response_fork(request) })
                .await;
            let _ = waku.update(cx, move |waku, cx| {
                waku.finish_response_fork(session_id, turn_count, provider, result, cx);
            });
        })
        .detach();
    }

    fn finish_response_fork(
        &mut self,
        session_id: Uuid,
        turn_count: usize,
        provider: ProviderId,
        result: Result<PreparedResponseFork, String>,
        cx: &mut Context<Self>,
    ) {
        if self.response_fork_preparations.get(&session_id) != Some(&turn_count) {
            return;
        }
        self.response_fork_preparations.remove(&session_id);
        let PreparedResponseFork {
            session,
            checkpoint_warning,
        } = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.show_toast(error);
                cx.notify();
                return;
            }
        };
        let fork_id = session.id;
        self.state.push_session(session);
        self.analytics
            .track(crate::analytics::Event::ResponseForked {
                provider: provider.as_str().to_owned(),
                turn_number: turn_count,
            });
        self.select_session(fork_id, cx);
        match checkpoint_warning {
            Some(error) => {
                self.show_toast(tr!("session.forked_with_checkpoint_warning", error = error))
            }
            None => self.show_success_toast(tr!("session.forked_from_response")),
        }
        cx.notify();
    }

    /// Composer Enter clears the field after emitting its event. A response
    /// fork temporarily owns the source provider, so restore a keyboard
    /// submission on the next task turn instead of racing it against the fork.
    pub(super) fn defer_restore_composer_after_fork(
        &self,
        session_id: Uuid,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        let composer = self.composer.clone();
        cx.spawn(async move |waku, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1))
                .await;
            let _ = waku.update(cx, |waku, cx| {
                if waku.state.selected_session == Some(session_id) {
                    composer.update(cx, |input, cx| {
                        if input.content().is_empty() {
                            input.set_content(prompt, cx);
                        }
                    });
                }
            });
        })
        .detach();
    }

    pub(super) fn begin_message_edit(
        &mut self,
        action: UserMessageAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let UserMessageAction {
            session_id,
            message_id,
            turn_count,
        } = action;
        let Some((message_index, initial_message, attachments)) = self
            .state
            .sessions
            .iter()
            .find(|session| {
                session.id == session_id
                    && matches!(session.status, SessionStatus::Idle | SessionStatus::Failed)
            })
            .and_then(|session| {
                let turn = session
                    .turns
                    .iter()
                    .find(|turn| turn.turn_count == turn_count)?;
                session
                    .messages
                    .iter()
                    .enumerate()
                    .find_map(|(index, message)| {
                        (message.id == message_id
                            && message.turn_id == Some(turn.id)
                            && message.role == MessageRole::User)
                            .then(|| {
                                (
                                    index,
                                    message.visible_content().to_owned(),
                                    message.attachments.clone(),
                                )
                            })
                    })
            })
        else {
            self.show_toast(tr!("session.message_not_editable"));
            cx.notify();
            return;
        };

        let input = cx.new(|cx| ComposerInput::new(window, cx).padding_x(px(12.0)));
        input.update(cx, |input, cx| input.set_content(initial_message, cx));
        cx.subscribe(
            &input,
            |this: &mut Self, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Submit(prompt) => {
                    this.submit_message_edit_prompt(prompt.clone(), cx)
                }
                // An edited past message resubmits from that point; there is
                // no running turn for it to steer.
                ComposerEvent::SubmitSteer(prompt) => {
                    this.submit_message_edit_prompt(prompt.clone(), cx)
                }
                ComposerEvent::Edited => cx.notify(),
                ComposerEvent::Focus => {}
                ComposerEvent::BackspaceOnEmpty => {}
            },
        )
        .detach();
        self.message_edit = Some(MessageEdit {
            session_id,
            message_id,
            turn_count,
            input: input.clone(),
            attachments,
        });
        self.hide_toast();
        self.remeasure_transcript_message(message_index);
        let focus_handle = input.read(cx).focus();
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    pub(super) fn cancel_message_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .message_edit
            .as_ref()
            .is_some_and(|edit| self.submission_preparations.contains(&edit.session_id))
        {
            return;
        }
        let Some(edit) = self.message_edit.take() else {
            return;
        };
        let message_index = self.selected_session().and_then(|session| {
            session
                .messages
                .iter()
                .position(|message| message.id == edit.message_id)
        });
        if let Some(message_index) = message_index {
            self.remeasure_transcript_message(message_index);
        }
        let focus_handle = self.composer_focus(cx);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    pub(super) fn submit_message_edit(&mut self, cx: &mut Context<Self>) {
        let prompt = self
            .message_edit
            .as_ref()
            .map(|edit| edit.input.read(cx).content().to_owned())
            .unwrap_or_default();
        self.submit_message_edit_prompt(prompt, cx);
    }

    fn submit_message_edit_prompt(&mut self, prompt: String, cx: &mut Context<Self>) {
        let Some(edit) = self.message_edit.clone() else {
            return;
        };
        if self.submission_preparations.contains(&edit.session_id) {
            return;
        }
        // Keyboard submission clears ComposerInput after emitting its event.
        // Use the event's captured value rather than rereading the field; the
        // button path enters here with its own pre-clear content as well.
        let prompt = prompt.trim().to_owned();
        if prompt.is_empty() && edit.attachments.is_empty() {
            self.show_toast(tr!("session.edited_message_empty"));
            cx.notify();
            return;
        }
        let mentions = edit
            .attachments
            .iter()
            .map(|attachment| attachment.mention.clone())
            .collect::<Vec<_>>();
        let provider_prompt = composer::merged_submission(&prompt, &mentions)
            .expect("edited text or retained attachments always form a submission");
        let display_content = (!edit.attachments.is_empty()).then_some(prompt);
        self.start_message_rewind(
            edit.clone(),
            ComposerSubmission {
                prompt: provider_prompt,
                display_content,
                attachments: edit.attachments,
            },
            cx,
        );
    }

    fn start_message_rewind(
        &mut self,
        edit: MessageEdit,
        submission: ComposerSubmission,
        cx: &mut Context<Self>,
    ) {
        let session_id = edit.session_id;
        let turn_count = edit.turn_count;
        let Some(source) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
        else {
            self.show_toast(tr!("session.message_unavailable"));
            cx.notify();
            return;
        };
        if self.state.selected_session != Some(session_id)
            || !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed)
            || !source
                .turns
                .iter()
                .any(|turn| turn.turn_count == turn_count)
        {
            self.show_toast(tr!("session.select_before_rewind"));
            cx.notify();
            return;
        }
        let edited_message_id = edit.message_id;
        let Some(edited_message_index) = source
            .turns
            .iter()
            .find(|turn| turn.turn_count == turn_count)
            .and_then(|turn| {
                source.messages.iter().position(|message| {
                    message.id == edited_message_id
                        && message.turn_id == Some(turn.id)
                        && message.role == MessageRole::User
                })
            })
        else {
            self.show_toast(tr!("session.message_unavailable"));
            cx.notify();
            return;
        };
        let request = MessageRewindRequest {
            daemon_client: self.daemon.client(),
            session_id,
            turn_count,
        };
        let original_message = self.state.session_mut(session_id).and_then(|session| {
            let message = session
                .messages
                .iter_mut()
                .find(|message| message.id == edited_message_id)?;
            let original = message.clone();
            message.content = submission.prompt.clone();
            message.display_content = submission.display_content.clone();
            message.attachments = submission.attachments.clone();
            session.status = SessionStatus::Connecting;
            session.updated_at = unix_time();
            Some(original)
        });
        let Some(original_message) = original_message else {
            self.show_toast(tr!("session.message_unavailable"));
            cx.notify();
            return;
        };
        let previous_status = source.status;
        self.message_edit = None;
        self.submission_preparations.insert(session_id);
        self.hide_toast();
        self.remeasure_transcript_message(edited_message_index);
        cx.notify();
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { perform_message_rewind(request) })
                .await;
            let _ = waku.update(cx, move |waku, cx| {
                waku.finish_message_rewind(
                    MessageRewindCompletion {
                        edit,
                        submission,
                        edited_message_id,
                        original_message,
                        previous_status,
                        result,
                    },
                    cx,
                );
            });
        })
        .detach();
    }

    fn finish_message_rewind(
        &mut self,
        completion: MessageRewindCompletion,
        cx: &mut Context<Self>,
    ) {
        let MessageRewindCompletion {
            edit,
            submission,
            edited_message_id,
            original_message,
            previous_status,
            result,
        } = completion;

        let session_id = edit.session_id;
        if !self.submission_preparations.remove(&session_id) {
            return;
        }
        let selected = self.state.selected_session == Some(session_id);
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(session) = self.state.session_mut(session_id) {
                    if let Some(message) = session
                        .messages
                        .iter_mut()
                        .find(|message| message.id == edited_message_id)
                    {
                        *message = original_message;
                    }
                    if session.status == SessionStatus::Connecting {
                        session.status = previous_status;
                    }
                }
                if selected && self.message_edit.is_none() {
                    self.message_edit = Some(edit);
                }
                if selected
                    && let Some(message_index) = self.selected_session().and_then(|session| {
                        session
                            .messages
                            .iter()
                            .position(|message| message.id == edited_message_id)
                    })
                {
                    self.remeasure_transcript_message(message_index);
                }
                self.show_toast(error);
                cx.notify();
                return;
            }
        };
        let provider = prepared.session.provider.clone();
        if let Some(runtime) = self.runtimes.remove(&session_id) {
            runtime.driver.close();
        }
        self.mark_background_work_lost(session_id);
        if let Some(session) = self.state.session_mut(session_id) {
            *session = prepared.session;
        }
        self.invalidate_checkpoint_refs();
        self.message_edit = None;
        if selected {
            self.sync_transcript_rows();
            self.show_toast(match prepared.cleanup_error {
                None => tr!("session.rewound", turn = edit.turn_count),
                Some(error) => tr!(
                    "session.rewound_with_stale_refs",
                    turn = edit.turn_count,
                    error = error
                ),
            });
        }
        self.analytics
            .track(crate::analytics::Event::ConversationRolledBack {
                provider: provider.as_str().to_owned(),
                turns: 1,
            });
        cx.notify();
        self.submit_submission_for_session(session_id, submission, cx);
    }

    /// Resolves the options sent to the daemon-owned endpoint runtime.
    fn service_tier_for_session(
        &self,
        session: &AgentSession,
    ) -> Option<wakuwaku_client::ServiceTier> {
        let model = self.model_for_session(session)?;
        gated_service_tier(
            session.service_tier,
            catalog_allows_service_tier(catalog_entry_for(
                &self.model_catalogs,
                &session.provider,
                model,
            )),
        )
    }

    pub(super) fn reasoning_effort_for_session(&self, session: &AgentSession) -> Option<String> {
        let model = self.model_for_session(session)?;
        provider_reasoning_effort(
            session.reasoning_effort.as_deref(),
            catalog_entry_for(&self.model_catalogs, &session.provider, model),
        )
        .map(str::to_owned)
    }

    pub(super) fn session_options(&self, session: &AgentSession) -> SessionOptions {
        SessionOptions {
            mode: session.runtime_mode,
            interaction_mode: session.interaction_mode,
            model: super::composer::send_provider_model(
                &session.provider,
                session.model.as_deref(),
                &self.model_catalogs,
            )
            .map(|(_, model)| model),
            reasoning_effort: self.reasoning_effort_for_session(session),
            service_tier: self.service_tier_for_session(session),
            context_window: session.context_window.clone(),
        }
    }

    pub(super) fn reap_idle_sessions(&mut self) {
        if self.last_idle_session_sweep.elapsed() < IDLE_SESSION_SWEEP_INTERVAL {
            return;
        }
        self.last_idle_session_sweep = Instant::now();
        let idle = self
            .runtimes
            .iter()
            .filter(|(session_id, runtime)| {
                let session = self
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == **session_id);
                session_is_reapable(
                    session,
                    runtime.last_active_at.elapsed(),
                    self.session_has_live_background_work(**session_id),
                )
            })
            .map(|(session_id, _)| *session_id)
            .collect::<Vec<_>>();
        for session_id in idle {
            if let Some(runtime) = self.runtimes.remove(&session_id) {
                runtime.driver.close();
            }
        }
    }

    pub(super) fn apply_session_options(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let Some(options) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| self.session_options(session))
        else {
            return;
        };
        let Some(runtime) = self.runtimes.get_mut(&session_id) else {
            return;
        };
        runtime.options_generation = runtime.options_generation.wrapping_add(1);
        let generation = runtime.options_generation;
        let driver = runtime.driver.clone();
        cx.spawn(async move |waku, cx| {
            let applied = cx
                .background_executor()
                .spawn(async move { driver.apply_options(options) })
                .await;
            let _ = waku.update(cx, |waku, cx| {
                let is_current = waku
                    .runtimes
                    .get(&session_id)
                    .is_some_and(|runtime| runtime.options_generation == generation);
                if is_current && !applied {
                    waku.reset_session_runtime(session_id);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn driver_start_request_for_session(
        &self,
        session: &AgentSession,
        cwd: PathBuf,
    ) -> anyhow::Result<DriverStartRequest> {
        let Some(endpoint) =
            provider_endpoint_for_start(&session.provider, &self.state.external_providers)
        else {
            anyhow::bail!("provider {} is not configured", session.provider.as_str());
        };
        endpoint.id.validate().map_err(anyhow::Error::msg)?;
        if endpoint.default_model.trim().is_empty() {
            anyhow::bail!("provider {:?} has no default model", endpoint.id.as_str());
        }
        let SessionOptions {
            mode,
            interaction_mode,
            model,
            reasoning_effort,
            service_tier,
            context_window,
        } = self.session_options(session);
        Ok(DriverStartRequest {
            session_id: session.id,
            options: DriverStartOptions {
                provider: session.provider.clone(),
                cwd,
                mode,
                interaction_mode,
                model,
                reasoning_effort,
                service_tier,
                context_window,
            },
            task: wakuwaku_client::StartTask {
                session: session.clone(),
                project: self
                    .state
                    .projects
                    .iter()
                    .find(|project| project.id == session.project_id)
                    .cloned(),
                generation: session.transcript_baseline_generation(),
            },
            event_wake: self.event_wake_tx.clone(),
            daemon_client: self.daemon.client(),
        })
    }

    fn install_prepared_driver(
        &mut self,
        session_id: Uuid,
        prepared: PreparedDriver,
        cx: &mut Context<Self>,
    ) -> DriverHandle {
        let handle = prepared.handle.clone();
        let runtime_id = handle.runtime_id();
        self.runtimes.insert(
            session_id,
            SessionRuntime {
                driver: prepared.handle,
                options_generation: 0,
                events: prepared.events,
                pending_events: VecDeque::new(),
                pending_steers: VecDeque::new(),
                stream_phase: None,
                stream_remeasure_pending: false,
                pending_permission: None,
                pending_user_input: None,
                last_driver_error: None,
                last_active_at: Instant::now(),
            },
        );
        // Startup can emit before the background task hands this receiver to
        // the runtime map. Wake once after installation so those buffered
        // events cannot be stranded behind an already-consumed edge.
        signal_event_pump(&self.event_wake_tx);

        let daemon_client = self.daemon.client();
        cx.spawn(async move |waku, cx| {
            let trajectory_client = wakuwaku_client::TrajectoryClient::new(daemon_client);
            let rx = trajectory_client.subscribe(session_id, runtime_id);
            loop {
                let rx_clone = rx.clone();
                let update = cx
                    .background_executor()
                    .spawn(async move { rx_clone.recv() })
                    .await;
                match update {
                    Ok(update) => {
                        let _ = waku.update(cx, |waku, cx| {
                            waku.apply_trajectory_live_update(session_id, update, cx);
                        });
                    }
                    Err(_) => break,
                }
            }
        })
        .detach();

        handle
    }

    pub(super) fn submit_composer_submission(
        &mut self,
        submission: ComposerSubmission,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected_session() else {
            return;
        };
        if self.response_fork_preparations.contains_key(&session.id) {
            return;
        }
        if session.is_busy() {
            // While the agent is working, Enter queues a follow-up instead of
            // refusing the message. The queue drains once the turn settles.
            self.enqueue_follow_up_submission(session.id, submission, cx);
            return;
        }
        self.submit_submission_for_session(session.id, submission, cx);
    }

    /// Deliver a steering message into the running turn. Providers without a
    /// live-turn transport (or a session that is not actively working) fall
    /// back to queueing a follow-up.
    pub(super) fn steer_composer_submission(
        &mut self,
        submission: ComposerSubmission,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if !session.is_busy() {
            self.submit_composer_submission(submission, cx);
            return;
        }
        // A turn that has not reached the provider yet cannot be steered; the
        // driver reports the outcome asynchronously via SteerAccepted or
        // SteerRejected once it is handed off.
        let steerable = session.status != SessionStatus::Connecting
            && self
                .runtimes
                .get(&session.id)
                .is_some_and(|runtime| runtime.driver.supports_steer());
        if !steerable {
            self.enqueue_follow_up_submission(session.id, submission, cx);
            return;
        }
        if let Some(runtime) = self.runtimes.get_mut(&session.id) {
            runtime
                .driver
                .steer(prompt_input_from_submission(&submission));
            runtime.pending_steers.push_back(submission);
        } else {
            self.enqueue_follow_up_submission(session.id, submission, cx);
        }
        cx.notify();
    }

    pub(super) fn enqueue_follow_up_submission(
        &mut self,
        session_id: Uuid,
        mut submission: ComposerSubmission,
        cx: &mut Context<Self>,
    ) {
        submission.prompt = submission.prompt.trim().to_owned();
        if submission.prompt.is_empty() {
            return;
        }
        if let Some(session) = self.state.session_mut(session_id) {
            session
                .queued_messages
                .push(submission.into_queued_message());
            session.updated_at = unix_time();
        }
        self.save();
        cx.notify();
    }

    pub(super) fn remove_queued_message(
        &mut self,
        session_id: Uuid,
        message_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.state.session_mut(session_id) {
            session
                .queued_messages
                .retain(|message| message.id != message_id);
        }
        self.save();
        cx.notify();
    }

    /// Pop a queued message back into the composer so the user can edit and
    /// resubmit it.
    pub(super) fn edit_queued_message(
        &mut self,
        session_id: Uuid,
        message_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(message) = self.state.session_mut(session_id).and_then(|session| {
            let index = session
                .queued_messages
                .iter()
                .position(|message| message.id == message_id)?;
            Some(session.queued_messages.remove(index))
        }) else {
            return;
        };
        self.restore_composer_submission(ComposerSubmission::from_queued_message(message), cx);
        let focus_handle = self.composer_focus(cx);
        window.focus(&focus_handle, cx);
        self.save();
        cx.notify();
    }

    /// Deliver a queued follow-up into the running turn right away instead of
    /// waiting for the turn to settle. Falls through the same paths as a
    /// composer steer: an idle session starts a fresh turn, an unsteerable
    /// one re-queues the message.
    pub(super) fn steer_queued_message(
        &mut self,
        session_id: Uuid,
        message_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let Some(message) = self.state.session_mut(session_id).and_then(|session| {
            let index = session
                .queued_messages
                .iter()
                .position(|message| message.id == message_id)?;
            Some(session.queued_messages.remove(index))
        }) else {
            return;
        };
        self.save();
        self.steer_composer_submission(ComposerSubmission::from_queued_message(message), cx);
    }

    /// Start the next queued follow-up as a fresh turn. Only called once a
    /// settled turn has been fully closed, so the session is Idle.
    fn drain_queued_message(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if self.response_fork_preparations.contains_key(&session_id) {
            return;
        }
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        if session.is_busy()
            || session.queued_messages.is_empty()
            || self.ending_checkpoint_pending(session_id)
        {
            return;
        }
        let Some(message) = self
            .state
            .session_mut(session_id)
            .map(|session| session.queued_messages.remove(0))
        else {
            return;
        };
        self.submit_submission_for_session(
            session_id,
            ComposerSubmission::from_queued_message(message),
            cx,
        );
    }

    fn submit_submission_for_session(
        &mut self,
        session_id: Uuid,
        submission: ComposerSubmission,
        cx: &mut Context<Self>,
    ) {
        if self.response_fork_preparations.contains_key(&session_id) {
            return;
        }
        let selected = self.state.selected_session == Some(session_id);
        let Some(session) = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        if self.ending_checkpoint_pending(session_id) {
            self.enqueue_follow_up_submission(session_id, submission, cx);
            self.defer_queue_drain(session_id);
            return;
        }
        if session.status.is_busy() {
            self.enqueue_follow_up_submission(session_id, submission, cx);
            return;
        }
        let prompt = submission.prompt.clone();
        let human_prompt = submission.human_prompt();
        let has_input = !submission
            .display_content
            .as_deref()
            .unwrap_or(&submission.prompt)
            .trim()
            .is_empty();
        let next_turn_count = session.turns.len() + 1;
        let provider = session.provider.as_str().to_owned();
        let model = self
            .session_options(session)
            .model
            .unwrap_or_else(|| "default".into());
        let workspace_kind = if session.workspace.is_worktree() {
            "worktree"
        } else {
            "local"
        };
        let attachment_count = submission.attachments.len();
        let project_id = session.project_id;
        let workspace = session.workspace.clone();
        let driver_start = (!self.runtimes.contains_key(&session_id)).then(|| {
            let provisional_cwd = self
                .workspace_path_for_session(session)
                .map(std::path::Path::to_path_buf)
                .unwrap_or_default();
            self.driver_start_request_for_session(session, provisional_cwd)
        });
        let Some(project) = self
            .state
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .cloned()
        else {
            if selected {
                self.restore_composer_submission(submission, cx);
                self.show_toast(tr!("errors.prepare_task_project_not_found"));
            }
            cx.notify();
            return;
        };
        let projectless = project.is_projectless();
        // Busy is visible before any Git work begins. The separate transient
        // set keeps this non-cancellable phase visually distinct from a
        // connecting provider, whose runtime already has a working Stop path.
        //
        // The turn also begins now, not once preparation settles: the sent
        // message and its working indicator belong in the transcript the
        // moment the submission is accepted — a first prompt otherwise leaves
        // the empty state on screen for as long as a `git add -A` takes.
        // Preparation failure unwinds the turn and restores the prompt.
        if selected {
            self.sync_transcript_rows();
        }
        let previous_kinds = if selected {
            self.transcript_row_kinds.borrow().clone()
        } else {
            Vec::new()
        };
        let transcript_anchor = if let Some(session) = self.state.session_mut(session_id) {
            session.set_title_from_prompt(&human_prompt);
            let turn_id = session.begin_turn_with_presentation(
                &prompt,
                submission.display_content.clone(),
                submission.attachments.clone(),
            );
            session.status = SessionStatus::Connecting;
            session.updated_at = unix_time();
            selected.then_some(TranscriptAnchor {
                session_id,
                turn_id,
            })
        } else {
            None
        };
        self.analytics
            .track(crate::analytics::Event::TurnSubmitted {
                provider,
                model,
                turn_number: next_turn_count,
                workspace: workspace_kind,
                projectless,
                attachment_count,
                has_input,
            });
        self.submission_preparations.insert(session_id);
        if selected {
            self.activities_expanded.clear();
            self.expanded_activity_items.clear();
            self.expanded_turns.clear();
            self.expanded_changed_files.clear();
            self.transcript_control_focuses.borrow_mut().clear();
            self.message_edit = None;
            self.hide_toast();
            self.transcript_anchor.set(transcript_anchor);
            // Provisional reservation: the anchored list has no measured
            // bounds until its first paint, and a zero end space cannot hold
            // the sent row at the viewport top — without scroll room past the
            // tail, the list clamps to its end and the prompt paints a frame
            // at the bottom before the first measured frame lifts it. Seed a
            // full viewport of end space instead; the overshoot is invisible
            // under the top anchor and the first measured frame trues it up.
            let mut provisional = self.transcript_rows.viewport_bounds().size.height;
            if provisional <= Pixels::ZERO {
                provisional = self.anchored_transcript_rows.viewport_bounds().size.height;
            }
            self.transcript_anchor_end_space.set(provisional);
            self.transcript_anchor_following.set(true);
            self.splice_transcript_rows_after_visibility_change(&previous_kinds);
            self.scroll_transcript_to_anchor();
        }
        cx.notify();

        let preparation_prompt = human_prompt;
        let workspace_client = wakuwaku_client::WorkspaceClient::new(self.daemon.client());
        cx.spawn(async move |waku, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    prepare_submission(
                        workspace_client,
                        project,
                        workspace,
                        driver_start,
                        session_id,
                        &preparation_prompt,
                        next_turn_count,
                    )
                })
                .await;
            let _ = waku.update(cx, move |waku, cx| {
                waku.finish_submission_preparation(session_id, submission, prepared, cx);
            });
        })
        .detach();
    }

    fn finish_submission_preparation(
        &mut self,
        session_id: Uuid,
        submission: ComposerSubmission,
        prepared: anyhow::Result<PreparedSubmission>,
        cx: &mut Context<Self>,
    ) {
        if !self.submission_preparations.contains(&session_id) {
            return;
        }
        let selected = self.state.selected_session == Some(session_id);
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.submission_preparations.remove(&session_id);
                self.track_active_turn_outcome(
                    session_id,
                    crate::analytics::TurnOutcome::PreparationFailed,
                );
                if selected {
                    self.sync_transcript_rows();
                }
                let previous_kinds = if selected {
                    self.transcript_row_kinds.borrow().clone()
                } else {
                    Vec::new()
                };
                if let Some(session) = self.state.session_mut(session_id)
                    && session.status == SessionStatus::Connecting
                {
                    // The submission never reached a provider and its prompt
                    // returns to the composer, so the eagerly-begun turn and
                    // its message leave the transcript with it.
                    if let Some(turn_id) = session.active_turn_id() {
                        session.unwind_unstarted_turn(turn_id);
                    }
                    session.status = SessionStatus::Idle;
                }
                if selected {
                    if self
                        .transcript_anchor
                        .get()
                        .is_some_and(|anchor| anchor.session_id == session_id)
                    {
                        self.transcript_anchor.set(None);
                        self.transcript_anchor_following.set(false);
                    }
                    self.splice_transcript_rows_after_visibility_change(&previous_kinds);
                    self.restore_composer_submission(submission, cx);
                    self.show_toast(tr!("errors.create_worktree", error = error));
                }
                cx.notify();
                return;
            }
        };
        let PreparedSubmission {
            workspace,
            checkpoint_warning,
            driver: prepared_driver,
        } = prepared;
        // The turn began at accept time; it must still be the untouched one
        // this preparation belongs to. Cancellation is blocked while the
        // preparation set holds the session, so a mismatch means the session
        // was replaced under the preparation rather than a user action.
        let can_start = self
            .state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(|session| {
                session.status == SessionStatus::Connecting
                    && session.turns.last().is_some_and(|turn| {
                        turn.status == TurnStatus::Running && !turn.provider_turn_started
                    })
            });
        if !can_start {
            self.submission_preparations.remove(&session_id);
            cx.notify();
            return;
        }

        let workspace_changed = self.state.session_mut(session_id).is_some_and(|session| {
            let changed = session.workspace != workspace;
            session.workspace = workspace;
            changed
        });
        if selected && workspace_changed {
            self.invalidate_workspace_queries(cx);
            self.reload_clean_right_panel_file_editors(cx);
            self.ensure_right_panel_terminals(cx);
        }
        let driver = match prepared_driver {
            None => self
                .runtimes
                .get(&session_id)
                .map(|runtime| runtime.driver.clone())
                .ok_or_else(|| anyhow::anyhow!(tr!("errors.prepared_runtime_unavailable"))),
            Some(Ok(prepared)) => Ok(self.install_prepared_driver(session_id, prepared, cx)),
            Some(Err(error)) => Err(error),
        };

        self.invalidate_checkpoint_refs();
        if let Some(runtime) = self.runtimes.get_mut(&session_id) {
            runtime
                .pending_events
                .retain(|event| matches!(event, DriverEvent::BackgroundWork(_)));
            runtime.pending_steers.clear();
            runtime.stream_remeasure_pending = false;
            runtime.stream_phase = None;
            runtime.pending_permission = None;
            runtime.pending_user_input = None;
            runtime.last_active_at = Instant::now();
        }
        // The transcript already shows the turn — the prompt message, its
        // anchor, and the working indicator all landed at accept time. Only
        // preparation's own output surfaces here.
        if selected && let Some(warning) = checkpoint_warning {
            self.show_toast(warning);
        }
        // Template commands expand here, at the seam between the transcript
        // and the transport: the user message keeps the typed `/name …`
        // while the embedded harness receives the rendered prompt.
        let prompt = submission.prompt.clone();
        let driver_prompt =
            crate::composer_complete::expanded_submission(&prompt, &self.slash_command_index)
                .unwrap_or(prompt);
        let mut failed_to_start = false;
        match driver {
            Ok(driver) => driver.prompt(prompt_input_from_submission_with_text(
                &submission,
                driver_prompt,
            )),
            Err(error) => {
                failed_to_start = true;
                let message = tr!("errors.start_agent", error = error);
                if let Some(session) = self.state.session_mut(session_id) {
                    session.status = SessionStatus::Failed;
                    session.push_message(MessageRole::Assistant, message);
                }
                self.finish_active_turn_with_analytics(
                    session_id,
                    TurnStatus::Failed,
                    crate::analytics::TurnOutcome::StartFailed,
                );
            }
        }
        // From this point onward `cancel_turn` has either a live driver to
        // cancel or a settled startup failure. The next frame must therefore
        // show Stop (or Send after failure), never the preparation spinner.
        self.submission_preparations.remove(&session_id);
        if failed_to_start {
            self.capture_latest_turn_checkpoint_for(session_id);
            self.start_pending_checkpoint_captures(cx);
        }
        cx.notify();
        // Persist on the next frame boundary. Saving is intentionally after
        // the spinner-to-Stop paint: SQLite or blob externalization must not
        // hold the final preparation frame motionless.
        cx.spawn(async move |waku, cx| {
            cx.background_executor().timer(STREAM_FRAME_INTERVAL).await;
            let _ = waku.update(cx, |waku, _| waku.save());
        })
        .detach();
    }

    pub(super) fn collect_runtime_events(runtime: &mut SessionRuntime) {
        while let Ok(event) = runtime.events.try_recv() {
            runtime.pending_events.push_back(event);
        }
    }

    pub(super) fn drain_event_pump(&mut self, cx: &mut Context<Self>) -> EventPumpSchedule {
        // `|` on purpose: a busy provider must not starve the other result
        // queues just because its own drain reported a change first.
        if self.drain_driver_events(cx) | self.drain_task_state_sync_events(cx) {
            cx.notify();
        }
        if std::mem::take(&mut self.workspace_queries_stale) {
            self.invalidate_workspace_queries(cx);
        }
        if std::mem::take(&mut self.composer_sources_stale) {
            self.refresh_composer_sources(cx);
        }
        self.maybe_refresh_background_work(cx);
        // A finished turn asks for a checkpoint from a handler with no
        // `Context`; this is where that `git` work leaves the UI thread.
        self.start_pending_checkpoint_captures(cx);

        if self
            .runtimes
            .values()
            .any(|runtime| !runtime.pending_events.is_empty() || runtime.stream_remeasure_pending)
        {
            EventPumpSchedule::StreamFrame
        } else if let Some(delay) = self.background_output_refresh_delay() {
            EventPumpSchedule::BackgroundOutput(delay)
        } else {
            EventPumpSchedule::Idle
        }
    }

    pub(super) fn drain_driver_events(&mut self, cx: &mut Context<Self>) -> bool {
        let session_ids = self.runtimes.keys().copied().collect::<Vec<_>>();
        let mut changed = false;
        let mut persisted_state_changed = false;
        let mut force_save = false;
        let mut selected_changed = false;
        for session_id in session_ids {
            let Some(mut runtime) = self.runtimes.remove(&session_id) else {
                continue;
            };
            let follow_up_remeasure = std::mem::take(&mut runtime.stream_remeasure_pending);
            Self::collect_runtime_events(&mut runtime);
            let mut runtime_changed = false;
            let mut background_changed = false;
            let mut markdown_changed = false;
            let mut keep_runtime = true;
            while let Some(event) = runtime.pending_events.front() {
                let kind = stream_delta_kind(event);
                let event = if let Some(kind) = kind {
                    pop_stream_batch(&mut runtime.pending_events, kind)
                } else {
                    runtime.pending_events.pop_front()
                };
                let Some(event) = event else {
                    break;
                };
                let background_event = matches!(&event, DriverEvent::BackgroundWork(_));
                let background_output_delta = matches!(
                    &event,
                    DriverEvent::BackgroundWork(work)
                        if matches!(work.as_ref(), BackgroundWorkEvent::OutputDelta { .. })
                );
                force_save |= matches!(
                    event,
                    DriverEvent::Connected
                        | DriverEvent::AutoTitleUpdated(_)
                        | DriverEvent::Permission { .. }
                        | DriverEvent::SteerAccepted { .. }
                        | DriverEvent::SteerRejected { .. }
                        | DriverEvent::TurnFinished { .. }
                        | DriverEvent::Error(_)
                        | DriverEvent::ProcessExited
                );
                // Reasoning is markdown too (the live peek renders it), and
                // this flag is also what routes the pump onto the coalesced
                // `StreamFrame` cadence: without it a reasoning-only drain
                // reported Idle, so every fast thinking chunk woke the pump
                // for an immediate drain-and-notify — 40+ full re-renders a
                // second, sailing straight past the 120 ms commit floor.
                markdown_changed |= matches!(
                    event,
                    DriverEvent::TextDelta(_) | DriverEvent::ReasoningDelta(_)
                );
                if background_output_delta {
                    // The registry batches log text into SharedString at 10Hz;
                    // repainting and saving for every provider chunk would
                    // turn a noisy command into UI-thread work.
                } else if background_event {
                    background_changed = true;
                } else {
                    runtime_changed = true;
                }
                keep_runtime &= self.handle_driver_event(session_id, &mut runtime, event, true, cx);
                if !keep_runtime {
                    break;
                }
            }
            runtime.stream_remeasure_pending = markdown_changed;
            if keep_runtime {
                self.runtimes.insert(session_id, runtime);
            }
            changed |= runtime_changed || background_changed;
            persisted_state_changed |= runtime_changed;
            if self.state.selected_session == Some(session_id)
                && (runtime_changed || follow_up_remeasure)
            {
                selected_changed = true;
            }
        }

        if !self.pending_queue_drains.is_empty() {
            let drains = std::mem::take(&mut self.pending_queue_drains);
            for session_id in drains {
                if self.ending_checkpoint_pending(session_id) {
                    self.defer_queue_drain(session_id);
                } else {
                    self.drain_queued_message(session_id, cx);
                }
            }
            changed = true;
        }

        if persisted_state_changed {
            self.stream_state_dirty = true;
        }
        if selected_changed {
            self.remeasure_transcript_tail();
        }
        if self.stream_state_dirty
            && (force_save || self.last_stream_save.elapsed() >= STREAM_SAVE_INTERVAL)
        {
            self.save();
        }
        changed || selected_changed
    }
}
pub(crate) fn catalog_entry_for<'a>(
    catalogs: &'a HashMap<ProviderId, wakuwaku_client::ModelCatalog>,
    provider: &ProviderId,
    model: &str,
) -> Option<&'a wakuwaku_client::ModelCatalogEntry> {
    catalogs
        .get(provider)?
        .models
        .iter()
        .find(|entry| entry.id == model)
}

pub(crate) fn catalog_allows_service_tier(
    entry: Option<&wakuwaku_client::ModelCatalogEntry>,
) -> bool {
    entry.is_some_and(|entry| entry.supported && entry.capabilities.service_tier)
}

pub(crate) fn catalog_allows_reasoning_effort(
    entry: Option<&wakuwaku_client::ModelCatalogEntry>,
) -> bool {
    entry.is_some_and(|entry| {
        entry.supported
            && entry.capabilities.reasoning_effort
            && !entry.reasoning_efforts.is_empty()
    })
}

pub(crate) fn gated_reasoning_effort<'a>(
    effort: Option<&'a str>,
    entry: Option<&wakuwaku_client::ModelCatalogEntry>,
) -> Option<&'a str> {
    let entry = entry.filter(|entry| catalog_allows_reasoning_effort(Some(entry)))?;
    effort.filter(|effort| {
        entry
            .reasoning_efforts
            .iter()
            .any(|candidate| candidate.id == *effort)
    })
}

pub(crate) fn provider_reasoning_effort<'a>(
    effort: Option<&str>,
    entry: Option<&'a wakuwaku_client::ModelCatalogEntry>,
) -> Option<&'a str> {
    let entry = entry.filter(|entry| catalog_allows_reasoning_effort(Some(entry)))?;
    entry.provider_reasoning_effort(effort?)
}

pub(crate) fn gated_service_tier(
    tier: Option<wakuwaku_client::ServiceTier>,
    catalog_allows: bool,
) -> Option<wakuwaku_client::ServiceTier> {
    tier.filter(|_| catalog_allows)
}
fn prompt_input_from_submission(submission: &ComposerSubmission) -> wakuwaku_client::PromptInput {
    prompt_input_from_submission_with_text(submission, submission.prompt.clone())
}
fn prompt_input_from_submission_with_text(
    submission: &ComposerSubmission,
    text: String,
) -> wakuwaku_client::PromptInput {
    let attachments = submission
        .attachments
        .iter()
        .filter(|attachment| attachment.is_image)
        .filter_map(|attachment| {
            attachment
                .blob_reference
                .as_deref()
                .and_then(wakuwaku_client::PromptImageRef::from_stored_reference)
        })
        .collect();
    let sources = submission
        .attachments
        .iter()
        .map(|attachment| {
            wakuwaku_client::PromptAttachmentSource::from_named_attachment(
                attachment.blob_reference.clone(),
                attachment.mention.clone(),
                attachment.name.clone(),
                attachment.is_dir,
                attachment.is_image,
            )
        })
        .collect();
    let display_text = submission
        .display_content
        .clone()
        .or_else(|| (text != submission.prompt).then(|| submission.prompt.clone()));
    wakuwaku_client::PromptInput {
        text,
        display_text,
        attachments,
        sources,
    }
}

pub(super) fn provider_endpoint_for_start(
    provider: &ProviderId,
    customs: &[ExternalProvider],
) -> Option<ExternalProvider> {
    if let Some(preset) = wakuwaku_client::ProviderPreset::parse_id(provider.as_str()) {
        return Some(preset.endpoint());
    }
    customs
        .iter()
        .find(|candidate| &candidate.id == provider)
        .cloned()
}

#[cfg(test)]
mod model_options_tests {
    use super::{
        catalog_allows_reasoning_effort, catalog_allows_service_tier, gated_reasoning_effort,
        gated_service_tier, provider_reasoning_effort,
    };
    use wakuwaku_client::{
        ApiFormat, ModelCapabilities, ModelCatalogEntry, ProviderId, ServiceTier, TransportProfile,
        UnsupportedReason,
    };

    fn entry(service_tier: bool, supported: bool) -> ModelCatalogEntry {
        ModelCatalogEntry {
            id: "gpt-5".into(),
            name: "gpt-5".into(),
            provider: ProviderId::new(ProviderId::OPENAI_RESPONSES),
            api_format: ApiFormat::OpenAiResponses,
            transport: TransportProfile::Standard,
            base_url: "https://api.openai.com/v1".into(),
            context_window: 128_000,
            max_output_tokens: 16_384,
            reasoning: false,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities: if service_tier {
                ModelCapabilities::openai_api(ApiFormat::OpenAiResponses)
            } else {
                ModelCapabilities::openai_compatible(ApiFormat::OpenAiResponses)
            },
            supported,
            unsupported_reason: (!supported).then_some(UnsupportedReason::NonChat),
        }
    }

    #[test]
    fn official_openai_catalog_entry_keeps_the_selected_tier() {
        assert_eq!(
            gated_service_tier(
                Some(ServiceTier::Priority),
                catalog_allows_service_tier(Some(&entry(true, true)))
            ),
            Some(ServiceTier::Priority)
        );
    }

    #[test]
    fn openai_compatible_catalog_entry_clears_the_tier() {
        assert_eq!(
            gated_service_tier(
                Some(ServiceTier::Flex),
                catalog_allows_service_tier(Some(&entry(false, true)))
            ),
            None
        );
    }

    #[test]
    fn reasoning_effort_requires_a_supported_catalog_choice() {
        let mut model = entry(false, true);
        model.reasoning_efforts = vec![
            wakuwaku_client::ReasoningEffortOption {
                id: "low".into(),
                provider_value: "provider-fast".into(),
                label: "Quick".into(),
            },
            wakuwaku_client::ReasoningEffortOption {
                id: "high".into(),
                provider_value: "provider-deep".into(),
                label: "Thorough".into(),
            },
        ];
        assert!(catalog_allows_reasoning_effort(Some(&model)));
        assert_eq!(
            gated_reasoning_effort(Some("high"), Some(&model)),
            Some("high")
        );
        assert_eq!(
            provider_reasoning_effort(Some("high"), Some(&model)),
            Some("provider-deep")
        );
        assert_eq!(gated_reasoning_effort(Some("medium"), Some(&model)), None);
        assert_eq!(
            provider_reasoning_effort(Some("medium"), Some(&model)),
            None
        );
        model.supported = false;
        assert!(!catalog_allows_reasoning_effort(Some(&model)));
    }

    #[test]
    fn missing_or_unsupported_catalog_entry_clears_the_tier() {
        assert_eq!(
            gated_service_tier(Some(ServiceTier::Flex), catalog_allows_service_tier(None)),
            None
        );
        assert_eq!(
            gated_service_tier(
                Some(ServiceTier::Priority),
                catalog_allows_service_tier(Some(&entry(true, false)))
            ),
            None
        );
    }
}

#[cfg(test)]
mod start_route_tests {
    use super::provider_endpoint_for_start;
    use crate::model::ExternalProvider;
    use wakuwaku_client::{ProviderId, ProviderPreset};

    #[test]
    fn every_builtin_preset_starts_without_a_custom_endpoint() {
        for preset in ProviderPreset::ALL {
            let endpoint = provider_endpoint_for_start(&preset.provider_id(), &[])
                .unwrap_or_else(|| panic!("{} must start without a custom endpoint", preset.id()));
            assert_eq!(endpoint.id, preset.provider_id());
            assert!(!endpoint.default_model.trim().is_empty(), "{}", preset.id());
            assert!(endpoint.base_url.starts_with("https://"), "{}", preset.id());
        }
    }

    #[test]
    fn unknown_provider_is_not_an_install_or_binary_error() {
        assert!(provider_endpoint_for_start(&ProviderId::new("mystery-cli"), &[]).is_none());
        let message = format!(
            "provider {} is not configured",
            ProviderId::new("mystery-cli")
        );
        assert!(!message.contains("installed"));
        assert!(!message.contains("尚未安装"));
        assert!(!message.contains("could not be found"));
    }

    #[test]
    fn custom_endpoints_still_start_by_id() {
        let custom = ExternalProvider::new(
            "corp",
            "Corp",
            "https://example.test/v1",
            Default::default(),
            "local-model",
        );
        let endpoint =
            provider_endpoint_for_start(&ProviderId::new("corp"), &[custom]).expect("custom");
        assert_eq!(endpoint.default_model, "local-model");
    }
}

#[cfg(test)]
mod prompt_input_tests {
    use super::{
        ComposerSubmission, prompt_input_from_submission, prompt_input_from_submission_with_text,
    };
    use std::path::PathBuf;
    use wakuwaku_client::{PromptAttachmentSource, PromptImageRef};
    use wakuwaku_protocol::model::MessageAttachment;
    fn attachment(reference: &str, mention: &str, name: &str, is_image: bool) -> MessageAttachment {
        MessageAttachment {
            path: PathBuf::from("/var/wakuwaku/attachments/secret/file"),
            mention: mention.to_owned(),
            name: name.to_owned(),
            is_dir: false,
            is_image,
            blob_reference: Some(reference.to_owned()),
        }
    }

    #[test]
    fn submission_keeps_display_text_and_safe_source_metadata() {
        let submission = ComposerSubmission {
            prompt: "see @notes.md".into(),
            display_content: Some("see".into()),
            attachments: vec![attachment(
                "wakuwaku-attachment:notes",
                "notes.md",
                "notes.md",
                false,
            )],
        };
        let input = prompt_input_from_submission(&submission);
        assert_eq!(input.text, "see @notes.md");
        assert_eq!(input.display_text.as_deref(), Some("see"));
        assert!(input.attachments.is_empty());
        assert_eq!(
            input.sources,
            vec![PromptAttachmentSource {
                reference: Some("wakuwaku-attachment:notes".into()),
                mention: "notes.md".into(),
                name: "notes.md".into(),
                is_dir: false,
                is_image: false,
                mime: None,
            }]
        );
        let json = serde_json::to_string(&input).unwrap();
        assert!(!json.contains("/var/waku"));
        assert!(!json.contains("base64"));
    }

    #[test]
    fn image_blob_stays_provider_facing_and_source_carries_mime() {
        let submission = ComposerSubmission {
            prompt: "inspect @shot.png".into(),
            display_content: Some("inspect".into()),
            attachments: vec![attachment(
                "wakuwaku-blob:shot.png",
                "shot.png",
                "shot.png",
                true,
            )],
        };
        let input = prompt_input_from_submission(&submission);
        assert_eq!(
            input.attachments,
            vec![PromptImageRef::Blob {
                reference: "wakuwaku-blob:shot.png".into()
            }]
        );
        assert_eq!(input.sources[0].mime.as_deref(), Some("image/png"));
        assert_eq!(
            input.sources[0].reference.as_deref(),
            Some("wakuwaku-blob:shot.png")
        );
    }

    #[test]
    fn expanded_provider_text_keeps_typed_display_text() {
        let submission = ComposerSubmission::plain("/review the diff".into());
        let input = prompt_input_from_submission_with_text(
            &submission,
            "Review the staged diff carefully".into(),
        );
        assert_eq!(input.text, "Review the staged diff carefully");
        assert_eq!(input.display_text.as_deref(), Some("/review the diff"));
    }

    #[test]
    fn host_path_and_data_url_are_not_provider_image_refs() {
        let submission = ComposerSubmission {
            prompt: "bad".into(),
            display_content: None,
            attachments: vec![
                attachment("/tmp/photo.png", "photo.png", "photo.png", true),
                attachment("data:image/png;base64,aaaa", "photo.png", "photo.png", true),
            ],
        };
        let input = prompt_input_from_submission(&submission);
        assert!(input.attachments.is_empty());
        assert!(
            input
                .sources
                .iter()
                .all(|source| source.reference.is_none())
        );
        assert_eq!(input.sources[0].mention, "photo.png");
        assert_eq!(input.sources[0].mime.as_deref(), Some("image/png"));
    }
}
