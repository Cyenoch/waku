use std::path::PathBuf;

use futures::StreamExt;
use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, Subscription, Task, Window, div,
    prelude::*, px,
};
use uuid::Uuid;
use wakuwaku_protocol::model::{
    AgentSession, InteractionMode, MessageRole, Project, ProviderId, RuntimeMode, SessionStatus,
};
use wakuwaku_protocol::{
    Command, PromptInput, ResponsePayload, SaveTaskState, SequencedEvent, StartTask,
    WireDriverStartOptions,
};

use crate::input::{ComposerEvent, ComposerInput};
use crate::md::render::{
    Ctx as MarkdownCtx, MarkdownView, Metrics as MarkdownMetrics, Palette as MarkdownPalette,
    TranscriptSelection, markdown,
};
use crate::theme::Theme;
use crate::ui::text_field::TextField;

use super::client::{NIL_UUID, WebDaemonClient};
use super::reduce::{TranscriptState, WebEvent, WebMessage, reduce_event};

const DEFAULT_ADDRESS: &str = "127.0.0.1:34123";
const DEFAULT_PROVIDER: &str = ProviderId::OPENAI_CODEX;

/// The browser-facing GPUI application.
///
/// The daemon remains authoritative for persisted tasks, provider runtimes, and
/// hydrated transcripts. This entity owns only the small projection needed by
/// the browser UI and forwards all task mutations through the protocol client.
pub struct WebApp {
    client: Option<WebDaemonClient>,
    address: Entity<ComposerInput>,
    token: Entity<ComposerInput>,
    composer: Entity<ComposerInput>,
    subscriptions: Vec<Subscription>,
    event_task: Option<Task<()>>,
    connecting: bool,
    error: Option<String>,
    projects: Vec<Project>,
    sessions: Vec<AgentSession>,
    default_cwd: PathBuf,
    selected_session_id: Option<Uuid>,
    selected_session: Option<AgentSession>,
    runtime_id: Option<Uuid>,
    supports_steer: bool,
    transcript: TranscriptState,
    markdown_views: Vec<MarkdownView>,
    reasoning_view: MarkdownView,
    selection: TranscriptSelection,
}

impl WebApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let address = cx.new(|cx| {
            let mut input = ComposerInput::new(window, cx)
                .search_field()
                .select_all_on_focus_click()
                .placeholder("Daemon address");
            input.set_content(DEFAULT_ADDRESS, cx);
            input
        });
        let token = cx.new(|cx| {
            ComposerInput::new(window, cx)
                .secret_field()
                .placeholder("Token (optional)")
        });
        let composer = cx.new(|cx| {
            ComposerInput::new(window, cx).placeholder("Ask WakuWaku anything…")
        });

        let mut app = Self {
            client: None,
            address,
            token,
            composer,
            subscriptions: Vec::new(),
            event_task: None,
            connecting: false,
            error: None,
            projects: Vec::new(),
            sessions: Vec::new(),
            default_cwd: PathBuf::new(),
            selected_session_id: None,
            selected_session: None,
            runtime_id: None,
            supports_steer: false,
            transcript: TranscriptState::default(),
            markdown_views: Vec::new(),
            reasoning_view: MarkdownView::new(),
            selection: TranscriptSelection::default(),
        };

        let composer_subscription = cx.subscribe(
            &app.composer,
            |this: &mut Self, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Submit(prompt) => this.submit_prompt(prompt.clone(), false, cx),
                ComposerEvent::SubmitSteer(prompt) if this.supports_steer => {
                    this.submit_prompt(prompt.clone(), true, cx)
                }
                ComposerEvent::SubmitSteer(_) => {}
                ComposerEvent::Edited
                | ComposerEvent::Focus
                | ComposerEvent::BackspaceOnEmpty => {}
            },
        );
        app.subscriptions.push(composer_subscription);
        app
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.connecting || self.client.is_some() {
            return;
        }
        let address = self.address.read(cx).content().trim().to_owned();
        let token = self.token.read(cx).content().to_owned();
        if address.is_empty() {
            self.error = Some("Enter a daemon address.".to_owned());
            cx.notify();
            return;
        }

        self.connecting = true;
        self.error = None;
        cx.notify();

        let foreground = cx.foreground_executor().clone();
        let background = cx.background_executor().clone();
        let task = WebDaemonClient::connect(&address, token, Vec::new(), foreground, background);
        cx.spawn(async move |this, cx| {
            match task.await {
                Ok(client) => {
                    let _ = this.update(cx, |this, cx| {
                        this.connecting = false;
                        this.client = Some(client.clone());
                        this.error = None;
                        this.load_task_state(client, cx);
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.connecting = false;
                        this.error = Some(error.to_string());
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn load_task_state(&mut self, client: WebDaemonClient, cx: &mut Context<Self>) {
        let task = client.request(NIL_UUID, NIL_UUID, Command::LoadTaskState);
        cx.spawn(async move |this, cx| {
            match task.await {
                Ok(ResponsePayload::TaskState {
                    projects,
                    mut sessions,
                    default_cwd,
                    ..
                }) => {
                    for session in &mut sessions {
                        session.detail_loaded = false;
                    }
                    let _ = this.update(cx, |this, cx| {
                        this.projects = projects;
                        this.sessions = sessions;
                        this.default_cwd = default_cwd;
                        this.error = None;
                        cx.notify();
                    });
                }
                Ok(_) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some("Daemon returned invalid task state.".to_owned());
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(error.message);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn attach_session(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };

        self.stop_event_stream();
        self.selected_session_id = Some(session_id);
        self.selected_session = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned();
        self.runtime_id = None;
        self.supports_steer = false;
        self.transcript = TranscriptState::default();
        self.markdown_views.clear();
        self.reasoning_view = MarkdownView::new();
        self.error = None;
        cx.notify();

        let task = client.request(session_id, NIL_UUID, Command::AttachSession);
        cx.spawn(async move |this, cx| {
            match task.await {
                Ok(ResponsePayload::SessionRuntime {
                    runtime_id,
                    supports_steer,
                }) => {
                    let _ = this.update(cx, |this, cx| {
                        this.runtime_id = runtime_id;
                        this.supports_steer = supports_steer;
                        this.subscribe_and_hydrate(client.clone(), session_id, runtime_id, cx);
                    });
                }
                Ok(_) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some("Daemon returned invalid runtime attachment.".to_owned());
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(error.message);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn subscribe_and_hydrate(
        &mut self,
        client: WebDaemonClient,
        session_id: Uuid,
        runtime_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        self.stop_event_stream();
        if let Some(runtime_id) = runtime_id {
            let events = client.subscribe(session_id, runtime_id);
            self.event_task = Some(cx.spawn(async move |this, cx| {
                let mut events = events;
                while let Some(event) = events.next().await {
                    if this
                        .update(cx, |this, cx| this.apply_event(event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }

        let task = client.request(
            session_id,
            runtime_id.unwrap_or_else(Uuid::nil),
            Command::HydrateSession { session_id },
        );
        cx.spawn(async move |this, cx| {
            match task.await {
                Ok(ResponsePayload::Session { session: Some(session) }) => {
                    let _ = this.update(cx, |this, cx| {
                        this.selected_session = Some(*session.clone());
                        if let Some(row) = this
                            .sessions
                            .iter_mut()
                            .find(|row| row.id == session.id)
                        {
                            *row = session.list_projection();
                        }
                        this.transcript.hydrate(&session);
                        this.sync_markdown_views();
                        this.error = None;
                        cx.notify();
                    });
                }
                Ok(ResponsePayload::Session { session: None }) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some("The selected session no longer exists.".to_owned());
                        cx.notify();
                    });
                }
                Ok(_) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some("Daemon returned invalid session data.".to_owned());
                        cx.notify();
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(error.message);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn stop_event_stream(&mut self) {
        self.event_task = None;
    }

    fn apply_event(&mut self, event: SequencedEvent, cx: &mut Context<Self>) {
        if self.selected_session_id != Some(event.session_id) {
            return;
        }
        let Some(reduced) = reduce_event(&event) else {
            return;
        };

        if let Some(session) = self.selected_session.as_mut() {
            match &reduced {
                WebEvent::AutoTitleUpdated(title) => session.auto_title = title.clone(),
                WebEvent::TurnStarted | WebEvent::TextDelta(_) | WebEvent::ReasoningDelta(_) => {
                    session.status = SessionStatus::Working;
                }
                WebEvent::TurnFinished { .. } => session.status = SessionStatus::Idle,
                WebEvent::Error(_) => session.status = SessionStatus::Failed,
                WebEvent::Connected => {}
            }
        }
        if let Some(row) = self
            .sessions
            .iter_mut()
            .find(|row| Some(row.id) == self.selected_session_id)
        {
            match &reduced {
                WebEvent::AutoTitleUpdated(title) => row.auto_title = title.clone(),
                WebEvent::TurnStarted | WebEvent::TextDelta(_) | WebEvent::ReasoningDelta(_) => {
                    row.status = SessionStatus::Working;
                }
                WebEvent::TurnFinished { .. } => row.status = SessionStatus::Idle,
                WebEvent::Error(_) => row.status = SessionStatus::Failed,
                WebEvent::Connected => {}
            }
        }

        self.transcript.apply(reduced);
        self.sync_markdown_views();
        cx.notify();
    }

    fn submit_prompt(&mut self, prompt: String, steer: bool, cx: &mut Context<Self>) {
        let prompt = prompt.trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        let Some(client) = self.client.clone() else {
            self.error = Some("Connect to a WakuWaku daemon first.".to_owned());
            cx.notify();
            return;
        };

        match (self.selected_session_id, self.runtime_id) {
            (Some(session_id), Some(runtime_id)) => {
                self.send_prompt(client, session_id, runtime_id, prompt, steer, cx);
            }
            (Some(_), None) if !steer => self.start_existing_task(client, prompt, cx),
            (Some(_), None) => {
                self.error = Some("The selected task has no running runtime.".to_owned());
                cx.notify();
            }
            (None, _) if !steer => self.start_new_task(client, prompt, cx),
            (None, _) => {}
        }
    }

    fn send_prompt(
        &mut self,
        client: WebDaemonClient,
        session_id: Uuid,
        runtime_id: Uuid,
        prompt: String,
        steer: bool,
        cx: &mut Context<Self>,
    ) {
        self.transcript
            .messages
            .push(WebMessage::new(MessageRole::User, prompt.clone()));
        self.transcript.error = None;
        self.sync_markdown_views();
        cx.notify();

        let command = if steer {
            Command::Steer {
                input: PromptInput::text(prompt),
            }
        } else {
            Command::Prompt {
                input: PromptInput::text(prompt),
            }
        };
        let task = client.request(session_id, runtime_id, command);
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                let message = error.message;
                let _ = this.update(cx, |this, cx| {
                    this.error = Some(message.clone());
                    this.transcript.error = Some(message);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn start_new_task(&mut self, client: WebDaemonClient, prompt: String, cx: &mut Context<Self>) {
        let project = self.new_task_project();
        let session = AgentSession::new(project.id, ProviderId::new(DEFAULT_PROVIDER));
        let mut projects = self.projects.clone();
        if !projects.iter().any(|existing| existing.id == project.id) {
            projects.push(project.clone());
        }
        let mut sessions = self.sessions.clone();
        sessions.push(session.list_projection());
        self.start_persisted_session(
            client,
            project,
            session,
            projects,
            sessions,
            prompt,
            true,
            cx,
        );
    }

    fn start_existing_task(&mut self, client: WebDaemonClient, prompt: String, cx: &mut Context<Self>) {
        let Some(session) = self.selected_session.clone() else {
            self.start_new_task(client, prompt, cx);
            return;
        };
        let project = self.project_for_session(&session);
        self.start_persisted_session(
            client,
            project,
            session,
            self.projects.clone(),
            self.sessions.clone(),
            prompt,
            false,
            cx,
        );
    }

    fn start_persisted_session(
        &mut self,
        client: WebDaemonClient,
        project: Project,
        session: AgentSession,
        projects: Vec<Project>,
        mut sessions: Vec<AgentSession>,
        prompt: String,
        is_new: bool,
        cx: &mut Context<Self>,
    ) {
        if is_new {
            sessions.retain(|existing| existing.id != session.id);
            sessions.push(session.list_projection());
        }
        let save = client.request(
            NIL_UUID,
            NIL_UUID,
            Command::SaveTaskState(SaveTaskState::boxed(
                projects.clone(),
                sessions
                    .iter()
                    .filter(|session| session.status.is_busy())
                    .map(|session| session.id)
                    .collect(),
                sessions,
            )),
        );
        let runtime_id = Uuid::new_v4();
        let options = Self::start_options(&session, &project);
        let transport = client.clone();

        cx.spawn(async move |this, cx| {
            if let Err(error) = save.await {
                let message = error.message;
                let _ = this.update(cx, |this, cx| {
                    this.error = Some(message);
                    cx.notify();
                });
                return;
            }

            let started = transport
                .request(
                    session.id,
                    runtime_id,
                    Command::Start { options },
                )
                .await;
            let supports_steer = match started {
                Ok(ResponsePayload::Started { supports_steer, .. }) => supports_steer,
                Ok(_) => {
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some("Daemon returned invalid start response.".to_owned());
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let message = error.message;
                    let _ = this.update(cx, |this, cx| {
                        this.error = Some(message);
                        cx.notify();
                    });
                    return;
                }
            };

            let session_id = session.id;
            let prompt_for_request = prompt.clone();
            let _ = this.update(cx, |this, cx| {
                this.projects = projects;
                this.sessions.retain(|row| row.id != session_id);
                this.sessions.push(session.list_projection());
                this.selected_session_id = Some(session_id);
                this.selected_session = Some(session.clone());
                this.runtime_id = Some(runtime_id);
                this.supports_steer = supports_steer;
                this.transcript.connected = true;
                this.transcript.title = Some(session.display_title().to_owned());
                this.transcript
                    .messages
                    .push(WebMessage::new(MessageRole::User, prompt.clone()));
                this.transcript.error = None;
                this.sync_markdown_views();
                this.subscribe_events(transport.clone(), session_id, runtime_id, cx);
                cx.notify();
            });

            let prompt_result = transport
                .request(
                    session_id,
                    runtime_id,
                    Command::Prompt {
                        input: PromptInput::text(prompt_for_request),
                    },
                )
                .await;
            if let Err(error) = prompt_result {
                let message = error.message;
                let _ = this.update(cx, |this, cx| {
                    this.error = Some(message.clone());
                    this.transcript.error = Some(message);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn start_options(session: &AgentSession, project: &Project) -> WireDriverStartOptions {
        let mode = match session.runtime_mode {
            RuntimeMode::Ask => "ask",
            RuntimeMode::AutoAcceptEdits => "autoAcceptEdits",
            RuntimeMode::FullAccess => "fullAccess",
        };
        let interaction_mode = match session.interaction_mode {
            InteractionMode::Build => "build",
            InteractionMode::Plan => "plan",
        };
        WireDriverStartOptions {
            provider: session.provider.clone(),
            cwd: project.path.clone(),
            mode: mode.to_owned(),
            interaction_mode: interaction_mode.to_owned(),
            model: session.model.clone(),
            reasoning_effort: session.reasoning_effort.clone(),
            service_tier: session.service_tier,
            context_window: session.context_window.clone(),
            task: Some(Box::new(StartTask {
                session: session.clone(),
                project: Some(project.clone()),
                generation: session.transcript_baseline_generation(),
            })),
        }
    }

    fn new_task_project(&self) -> Project {
        self.projects.first().cloned().unwrap_or_else(|| {
            let path = if self.default_cwd.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                self.default_cwd.clone()
            };
            Project::from_path(path)
        })
    }

    fn project_for_session(&self, session: &AgentSession) -> Project {
        self.projects
            .iter()
            .find(|project| project.id == session.project_id)
            .cloned()
            .unwrap_or_else(|| {
                let path = if self.default_cwd.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    self.default_cwd.clone()
                };
                let mut project = Project::from_path(path);
                project.id = session.project_id;
                project
            })
    }

    fn subscribe_events(
        &mut self,
        client: WebDaemonClient,
        session_id: Uuid,
        runtime_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.stop_event_stream();
        let mut events = client.subscribe(session_id, runtime_id);
        self.event_task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |this, cx| this.apply_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn new_task(&mut self, cx: &mut Context<Self>) {
        self.stop_event_stream();
        self.selected_session_id = None;
        self.selected_session = None;
        self.runtime_id = None;
        self.supports_steer = false;
        self.transcript = TranscriptState::default();
        self.markdown_views.clear();
        self.reasoning_view = MarkdownView::new();
        self.error = None;
        cx.notify();
    }

    fn sync_markdown_views(&mut self) {
        while self.markdown_views.len() < self.transcript.messages.len() {
            self.markdown_views.push(MarkdownView::seeded());
        }
        self.markdown_views.truncate(self.transcript.messages.len());
        for (view, message) in self
            .markdown_views
            .iter_mut()
            .zip(self.transcript.messages.iter())
        {
            view.set_text(
                &message.content,
                self.transcript.turn_in_progress && message.role == MessageRole::Assistant,
            );
        }
        self.reasoning_view
            .set_text(&self.transcript.reasoning, self.transcript.turn_in_progress);
    }

    fn render_connect(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let error = self.error.as_deref().map(|error| {
            div()
                .text_color(theme.danger)
                .text_size(px(12.0))
                .child(error.to_owned())
                .into_any_element()
        });
        let label = if self.connecting { "Connecting…" } else { "Connect" };
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .w(px(420.0))
            .p(px(28.0))
            .rounded(px(10.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .child(div().text_size(px(20.0)).child("WakuWaku Web"))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child("Connect to a running WakuWaku daemon."),
            )
            .child(TextField::new("daemon-address", self.address.clone()))
            .child(TextField::new("daemon-token", self.token.clone()))
            .child(
                div()
                    .id("connect-button")
                    .cursor(gpui::CursorStyle::PointingHand)
                    .px(px(12.0))
                    .py(px(7.0))
                    .rounded(px(6.0))
                    .bg(theme.inverse)
                    .text_color(theme.on_inverse)
                    .text_center()
                    .on_click(cx.listener(|this, _, _, cx| this.connect(cx)))
                    .child(label),
            )
            .children(error)
            .into_any_element()
    }

    fn render_transcript(&mut self, theme: Theme) -> AnyElement {
        let palette = MarkdownPalette::from_theme(&theme);
        let mut rows = Vec::new();
        for (index, message) in self.transcript.messages.iter().enumerate() {
            let (metrics, background, label) = match message.role {
                MessageRole::User => (MarkdownMetrics::USER_MESSAGE, theme.raised, "You"),
                MessageRole::Assistant => (MarkdownMetrics::BODY, theme.surface, "Assistant"),
                MessageRole::System => (MarkdownMetrics::COMPACT, theme.inset, "System"),
            };
            let context = MarkdownCtx::new(
                format!("web-message-{index}"),
                &palette,
                metrics,
                self.selection.clone(),
            )
            .with_streaming_animation(
                self.transcript.turn_in_progress && message.role == MessageRole::Assistant,
            );
            let body = markdown(&self.markdown_views[index], &context)
                .unwrap_or_else(|| div().into_any_element());
            rows.push(
                div()
                    .w_full()
                    .p(px(12.0))
                    .rounded(px(8.0))
                    .bg(background)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_tertiary)
                            .mb(px(5.0))
                            .child(label),
                    )
                    .child(body)
                    .into_any_element(),
            );
        }
        if !self.transcript.reasoning.is_empty() {
            let context = MarkdownCtx::new(
                "web-reasoning",
                &palette,
                MarkdownMetrics::COMPACT,
                self.selection.clone(),
            )
            .with_streaming_animation(self.transcript.turn_in_progress);
            if let Some(body) = markdown(&self.reasoning_view, &context) {
                rows.push(
                    div()
                        .w_full()
                        .p(px(10.0))
                        .rounded(px(8.0))
                        .bg(theme.inset)
                        .text_color(theme.text_secondary)
                        .child(div().text_size(px(11.0)).child("Reasoning"))
                        .child(body)
                        .into_any_element(),
                );
            }
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .text_color(theme.text_tertiary)
                    .child("Select a task or start a new one.")
                    .into_any_element(),
            );
        }
        div()
            .id("web-transcript")
            .flex()
            .flex_col()
            .gap(px(10.0))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .children(rows)
            .into_any_element()
    }

    fn render_workspace(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected_session_id;
        let session_rows = self
            .sessions
            .iter()
            .map(|session| {
                let active = selected == Some(session.id);
                let session_id = session.id;
                div()
                    .id(format!("session-{session_id}"))
                    .w_full()
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .when(active, |row| row.bg(theme.sidebar_item_background))
                    .text_color(if active {
                        theme.text
                    } else {
                        theme.text_secondary
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.attach_session(session_id, cx)
                    }))
                    .child(session.display_title().to_owned())
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let title = self
            .transcript
            .title
            .as_deref()
            .or_else(|| self.selected_session.as_ref().map(AgentSession::display_title))
            .unwrap_or("New task")
            .to_owned();
        let error = self.error.as_deref().map(|error| {
            div()
                .text_color(theme.danger)
                .text_size(px(12.0))
                .child(error.to_owned())
                .into_any_element()
        });

        div()
            .size_full()
            .flex()
            .bg(theme.canvas)
            .text_color(theme.text)
            .child(
                div()
                    .w(px(250.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .p(px(12.0))
                    .bg(theme.sidebar)
                    .border_r_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_size(px(16.0)).child("Tasks"))
                            .child(
                                div()
                                    .id("new-task")
                                    .cursor(gpui::CursorStyle::PointingHand)
                                    .text_color(theme.accent)
                                    .on_click(cx.listener(|this, _, _, cx| this.new_task(cx)))
                                    .child("New task"),
                            ),
                    )
                    .children(session_rows)
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_tertiary)
                            .child("Local workspace"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .p(px(18.0))
                    .gap(px(12.0))
                    .child(div().text_size(px(18.0)).child(title))
                    .child(self.render_transcript(theme))
                    .children(error)
                    .child(
                        div()
                            .w_full()
                            .rounded(px(8.0))
                            .bg(theme.composer)
                            .border_1()
                            .border_color(theme.border_strong)
                            .p(px(8.0))
                            .child(self.composer.clone()),
                    ),
            )
            .into_any_element()
    }
}

impl Render for WebApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(cx);
        if self.client.is_some() {
            self.render_workspace(theme, cx)
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.canvas)
                .text_color(theme.text)
                .child(self.render_connect(theme, cx))
                .into_any_element()
        }
    }
}
