//! Start restore + missing-snapshot lifecycle against a local mock HTTP provider.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;
use wakuwaku_client::driver::{DriverHandle, DriverStartOptions, event_channel};
use wakuwaku_client::{DaemonClient, PromptInput};
use wakuwaku_core::daemon::WakuBackend;
use wakuwaku_core::model::{
    DriverEvent, InteractionMode, MessageRole, ProviderId, RuntimeMode, TurnStatus,
};
use wakuwaku_core::persistence::{PersistedState, StateStore};
use wakuwaku_core::protocol::{Command, ResponsePayload, StartTask};
use wakuwaku_core::{DaemonSettings, DaemonSettingsStore, ServerOptions, serve};
use wakuwaku_core::auth::{
    AuthRuntime, AuthService, CredentialStore, MemoryCredentialStore, StoredCredential,
};
use wakuwaku_protocol::{ApiFormat, AuthEndpoints, ExternalProvider, SecretString};
const TOKEN: &str = "restore-token";
const PROVIDER: &str = "restore-openai";

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect()
}

struct MockHttp {
    responses: Mutex<VecDeque<String>>,
}

impl MockHttp {
    fn new(bodies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(bodies.into()),
        })
    }

    fn bind(self: &Arc<Self>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Arc::clone(self);
        thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mock = Arc::clone(&mock);
                        thread::spawn(move || serve_http(stream, &mock));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        port
    }
}

fn serve_http(mut stream: TcpStream, mock: &MockHttp) {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(at) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        if buf.len() > 1024 * 1024 {
            return;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&tmp[..n]);
    }
    let Some(body) = mock.responses.lock().pop_front() else {
        return;
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn model_sse(text: &str) -> String {
    sse(&[
        json!({"type":"response.created","response":{"id":"resp_restore"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":text}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"m","content":[{"type":"output_text","text":text}]}}),
        json!({"type":"response.completed","response":{"id":"resp_restore","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    ])
}

fn wait_event(
    rx: &crossbeam_channel::Receiver<DriverEvent>,
    timeout: Duration,
    mut pred: impl FnMut(&DriverEvent) -> bool,
) -> DriverEvent {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = rx
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("timed out waiting for driver event"));
        if pred(&event) {
            return event;
        }
    }
}

fn failed_pre_provider_session(
    project_id: Uuid,
    provider: &str,
) -> wakuwaku_core::model::AgentSession {
    let mut session =
        wakuwaku_core::model::AgentSession::new(project_id, ProviderId::new(provider));
    session.begin_turn("hi");
    session.push_message(
        MessageRole::Assistant,
        "无法启动智能体：the task is unavailable",
    );
    session.finish_active_turn(TurnStatus::Failed);
    session.status = wakuwaku_core::model::SessionStatus::Failed;
    session.model = Some("restore-model".into());
    session
}

fn start_daemon(
    directory: &std::path::Path,
    workspace: &std::path::Path,
    persist_session: bool,
) -> (
    DaemonClient,
    Uuid,
    wakuwaku_core::model::AgentSession,
    Arc<AtomicBool>,
) {
    let mock = MockHttp::new(vec![model_sse("restored")]);
    let port = mock.bind();
    let provider = ExternalProvider::new(
        PROVIDER,
        "Restore OpenAI",
        format!("http://127.0.0.1:{port}/v1"),
        ApiFormat::OpenAiResponses,
    );
    let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
    settings
        .replace(DaemonSettings {
            external_providers: vec![provider],
            extra: Default::default(),
        })
        .unwrap();
    let creds = Arc::new(MemoryCredentialStore::default());
    creds
        .set(
            &ProviderId::new(PROVIDER),
            StoredCredential::api_key(SecretString::new("sk-restore")),
        )
        .unwrap();
    let auth = AuthService::new(AuthRuntime::testing(
        &directory,
        creds,
        AuthEndpoints::production(),
    ))
    .unwrap();
    let store = StateStore::daemon(directory.join("app.db"));
    let mut state = PersistedState::fresh(workspace.to_path_buf());
    let mut session = failed_pre_provider_session(state.projects[0].id, PROVIDER);
    let session_id = session.id;
    if persist_session {
        state.sessions.clear();
        state.push_session(session.clone());
        store.save(&mut state).unwrap();
    } else {
        session.project_id = state.projects[0].id;
        state.sessions.clear();
        store.save(&mut state).unwrap();
    }
    let project = state.projects[0].clone();
    session.project_id = project.id;
    let backend = WakuBackend::new_with_auth(settings, store, auth).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = shutdown.clone();
    thread::spawn(move || {
        serve(
            listener,
            TOKEN.into(),
            Arc::new(backend),
            server_shutdown,
            ServerOptions {
                allow_shutdown: true,
                ..ServerOptions::default()
            },
        )
        .unwrap();
    });
    let client = DaemonClient::connect(&address.to_string(), TOKEN.into()).unwrap();
    let _ = project;
    (client, session_id, session, shutdown)
}

fn start_options(cwd: std::path::PathBuf) -> DriverStartOptions {
    DriverStartOptions {
        provider: ProviderId::new(PROVIDER),
        cwd,
        mode: RuntimeMode::Ask,
        interaction_mode: InteractionMode::Build,
        model: Some("restore-model".into()),
        reasoning_effort: None,
        service_tier: None,
        context_window: None,
    }
}

#[test]
fn persisted_pre_provider_failure_continues_over_mock_http() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-restore-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, session_id, session, shutdown) = start_daemon(&directory, &workspace, true);
    let (wake, _) = smol::channel::bounded(1);
    let (events, rx) = event_channel(wake);
    let handle = DriverHandle::start_restoring(
        client.clone(),
        session_id,
        start_options(workspace.clone()),
        Some(StartTask {
            generation: session.transcript_baseline_generation(),
            project: None,
            session,
        }),
        events,
    )
    .unwrap();
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::Connected)
    });
    handle.prompt(PromptInput::text("continue"));
    wait_event(
        &rx,
        Duration::from_secs(5),
        |event| matches!(event, DriverEvent::TextDelta(delta) if delta.contains("restored")),
    );
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::TurnFinished { success: true, .. })
    });
    drop(handle);
    client.shutdown();
    shutdown.store(true, Ordering::Release);
    let restored = StateStore::daemon(directory.join("app.db"));
    assert!(
        restored
            .load_harness_snapshot(session_id)
            .unwrap()
            .is_some()
    );
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn fresh_daemon_db_restores_app_local_session_over_mock_http() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-restore-fresh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, session_id, session, shutdown) = start_daemon(&directory, &workspace, false);
    let missing = client
        .request(
            session_id,
            Uuid::new_v4(),
            Command::Start {
                options: wakuwaku_core::protocol::WireDriverStartOptions {
                    provider: ProviderId::new(PROVIDER),
                    cwd: workspace.clone(),
                    mode: "ask".into(),
                    interaction_mode: "build".into(),
                    model: Some("restore-model".into()),
                    reasoning_effort: None,
                    service_tier: None,
                    context_window: None,
                    task: None,
                },
            },
        )
        .unwrap_err();
    assert!(
        missing.to_string().contains("the task is unavailable"),
        "{missing}"
    );

    let (wake, _) = smol::channel::bounded(1);
    let (events, rx) = event_channel(wake);
    let handle = DriverHandle::start_restoring(
        client.clone(),
        session_id,
        start_options(workspace),
        Some(StartTask {
            generation: session.transcript_baseline_generation(),
            project: None,
            session,
        }),
        events,
    )
    .expect("restored task should start");
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::Connected)
    });
    handle.prompt(PromptInput::text("hello"));
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::TurnFinished { success: true, .. })
    });
    drop(handle);
    client.shutdown();
    shutdown.store(true, Ordering::Release);
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn dropping_a_driver_handle_clone_keeps_prompt_events() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-handle-clone-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, session_id, session, shutdown) = start_daemon(&directory, &workspace, false);
    let (wake, _) = smol::channel::bounded(1);
    let (events, rx) = event_channel(wake);
    let handle = DriverHandle::start_restoring(
        client.clone(),
        session_id,
        start_options(workspace),
        Some(StartTask {
            generation: session.transcript_baseline_generation(),
            project: None,
            session,
        }),
        events,
    )
    .expect("task should start");
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::Connected)
    });
    // The desktop submit path clones the handle to call `prompt` and then
    // drops the clone. That must not unsubscribe the live event stream.
    drop(handle.clone());
    handle.prompt(PromptInput::text("hello"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = rx
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("timed out waiting for prompt events"));
        match event {
            DriverEvent::ProcessExited => {
                panic!("dropping a DriverHandle clone closed the event subscription")
            }
            DriverEvent::TurnFinished { success: true, .. } => break,
            _ => {}
        }
    }
    drop(handle);
    client.shutdown();
    shutdown.store(true, Ordering::Release);
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn start_failure_after_restore_still_has_snapshot() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-restore-fail-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
    let store = StateStore::daemon(directory.join("app.db"));
    let mut state = PersistedState::fresh(workspace.clone());
    state.sessions[0].begin_turn("first prompt");
    let session = state.sessions[0].clone();
    let session_id = session.id;
    store.save(&mut state).unwrap();
    let backend = WakuBackend::new(settings, store).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = shutdown.clone();
    thread::spawn(move || {
        serve(
            listener,
            TOKEN.into(),
            Arc::new(backend),
            server_shutdown,
            ServerOptions {
                allow_shutdown: true,
                ..ServerOptions::default()
            },
        )
        .unwrap();
    });
    let client = DaemonClient::connect(&address.to_string(), TOKEN.into()).unwrap();
    let error = client
        .request(
            session_id,
            Uuid::new_v4(),
            Command::Start {
                options: wakuwaku_core::protocol::WireDriverStartOptions {
                    provider: ProviderId::new("missing-provider"),
                    cwd: workspace,
                    mode: "ask".into(),
                    interaction_mode: "build".into(),
                    model: Some("no-model".into()),
                    reasoning_effort: None,
                    service_tier: None,
                    context_window: None,
                    task: Some(Box::new(StartTask {
                        generation: session.transcript_baseline_generation(),
                        project: None,
                        session,
                    })),
                },
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("not configured"), "{error}");
    client.shutdown();
    shutdown.store(true, Ordering::Release);
    let restored = StateStore::daemon(directory.join("app.db"));
    assert!(
        restored
            .load_harness_snapshot(session_id)
            .unwrap()
            .is_some(),
        "empty snapshot must exist after start failure"
    );
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn genuine_provider_termination_surfaces_real_failure() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-restore-term-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mock = MockHttp::new(Vec::new());
    let port = mock.bind();
    let provider = ExternalProvider::new(
        PROVIDER,
        "Restore OpenAI",
        format!("http://127.0.0.1:{port}/v1"),
        ApiFormat::OpenAiResponses,
    );
    let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
    settings
        .replace(DaemonSettings {
            external_providers: vec![provider],
            extra: Default::default(),
        })
        .unwrap();
    let store = StateStore::daemon(directory.join("app.db"));
    let mut state = PersistedState::fresh(workspace.clone());
    let session = failed_pre_provider_session(state.projects[0].id, PROVIDER);
    let session_id = session.id;
    state.sessions.clear();
    state.push_session(session.clone());
    store.save(&mut state).unwrap();
    let backend = WakuBackend::new(settings, store).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = shutdown.clone();
    thread::spawn(move || {
        serve(
            listener,
            TOKEN.into(),
            Arc::new(backend),
            server_shutdown,
            ServerOptions {
                allow_shutdown: true,
                ..ServerOptions::default()
            },
        )
        .unwrap();
    });
    let client = DaemonClient::connect(&address.to_string(), TOKEN.into()).unwrap();
    let (wake, _) = smol::channel::bounded(1);
    let (events, rx) = event_channel(wake);
    let handle = DriverHandle::start_restoring(
        client.clone(),
        session_id,
        start_options(workspace),
        Some(StartTask {
            generation: session.transcript_baseline_generation(),
            project: None,
            session,
        }),
        events,
    )
    .unwrap();
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::Connected)
    });
    handle.prompt(PromptInput::text("hi"));
    let mut saw_process_exit_first = false;
    let finished = wait_event(&rx, Duration::from_secs(8), |event| match event {
        DriverEvent::ProcessExited => {
            saw_process_exit_first = true;
            true
        }
        DriverEvent::TurnFinished { success: false, .. } | DriverEvent::Error(_) => true,
        _ => false,
    });
    assert!(
        !saw_process_exit_first,
        "provider HTTP failure must not be reported as ProcessExited"
    );
    match finished {
        DriverEvent::Error(message)
        | DriverEvent::TurnFinished {
            success: false,
            summary: Some(message),
        } => assert!(
            !message.trim().is_empty(),
            "genuine provider failure must carry a real error"
        ),
        other => panic!("unexpected completion {other:?}"),
    }
    drop(handle);
    client.shutdown();
    shutdown.store(true, Ordering::Release);
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn divergent_start_generation_does_not_prompt() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-restore-gen-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, session_id, session, shutdown) = start_daemon(&directory, &workspace, true);
    let (wake, _) = smol::channel::bounded(1);
    let (events, rx) = event_channel(wake);
    let error = match DriverHandle::start_restoring(
        client.clone(),
        session_id,
        start_options(workspace),
        Some(StartTask {
            generation: 10,
            project: None,
            session,
        }),
        events,
    ) {
        Ok(_) => panic!("divergent start generation must fail closed"),
        Err(error) => error,
    };
    let mismatch = error
        .downcast_ref::<wakuwaku_client::StartGenerationMismatch>()
        .unwrap_or_else(|| panic!("expected typed generation mismatch, got {error}"));
    assert_eq!(mismatch.submitted, 10);
    assert_ne!(mismatch.accepted, Some(10));
    assert!(
        rx.try_recv().is_err(),
        "divergent start must not connect a promptable runtime"
    );
    match client
        .request(session_id, Uuid::nil(), Command::AttachSession)
        .expect("attach after rejected start")
    {
        ResponsePayload::SessionRuntime {
            runtime_id: None, ..
        } => {}
        other => panic!("rejected start must close the daemon runtime, got {other:?}"),
    }

    client.shutdown();
    shutdown.store(true, Ordering::Release);
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn equal_timestamp_stale_client_closes_rejected_runtime() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-restore-eq-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mock = MockHttp::new(vec![model_sse("unused")]);
    let port = mock.bind();
    let provider = ExternalProvider::new(
        PROVIDER,
        "Restore OpenAI",
        format!("http://127.0.0.1:{port}/v1"),
        ApiFormat::OpenAiResponses,
    );
    let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
    settings
        .replace(DaemonSettings {
            external_providers: vec![provider],
            extra: Default::default(),
        })
        .unwrap();
    let store = StateStore::daemon(directory.join("app.db"));
    let mut state = PersistedState::fresh(workspace.clone());
    let mut existing =
        wakuwaku_core::model::AgentSession::new(state.projects[0].id, ProviderId::new(PROVIDER));
    existing.begin_turn("inspect");
    existing.mark_active_turn_provider_started();
    existing.push_message(MessageRole::Assistant, "calling");
    existing.finish_active_turn(TurnStatus::Completed);
    existing.begin_turn("continue");
    existing.mark_active_turn_provider_started();
    existing.push_message(MessageRole::Assistant, "done");
    existing.finish_active_turn(TurnStatus::Completed);
    existing.status = wakuwaku_core::model::SessionStatus::Idle;
    existing.updated_at = 50;
    existing.model = Some("restore-model".into());
    let session_id = existing.id;
    let daemon_generation = existing.transcript_baseline_generation();
    state.sessions.clear();
    state.push_session(existing.clone());
    store
        .persist_harness_snapshot(
            session_id,
            wakuwaku_harness::Session::new(Some("waku".into())).snapshot(),
        )
        .unwrap();
    store.save(&mut state).unwrap();

    let mut stale = existing;
    stale.turns.pop();
    stale.messages.truncate(2);
    stale.updated_at = 50;
    let submitted = stale.transcript_baseline_generation();
    assert_ne!(submitted, daemon_generation);

    let backend = WakuBackend::new(settings, store).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = shutdown.clone();
    thread::spawn(move || {
        serve(
            listener,
            TOKEN.into(),
            Arc::new(backend),
            server_shutdown,
            ServerOptions {
                allow_shutdown: true,
                ..ServerOptions::default()
            },
        )
        .unwrap();
    });
    let client = DaemonClient::connect(&address.to_string(), TOKEN.into()).unwrap();
    let (wake, _) = smol::channel::bounded(1);
    let (events, rx) = event_channel(wake);
    let error = match DriverHandle::start_restoring(
        client.clone(),
        session_id,
        start_options(workspace),
        Some(StartTask {
            generation: submitted,
            project: None,
            session: stale,
        }),
        events,
    ) {
        Ok(_) => panic!("equal-timestamp stale client must fail closed"),
        Err(error) => error,
    };
    let mismatch = error
        .downcast_ref::<wakuwaku_client::StartGenerationMismatch>()
        .unwrap_or_else(|| panic!("expected typed generation mismatch, got {error}"));
    assert_eq!(mismatch.submitted, submitted);
    assert_eq!(mismatch.accepted, Some(daemon_generation));
    assert!(rx.try_recv().is_err());
    match client
        .request(session_id, Uuid::nil(), Command::AttachSession)
        .expect("attach after rejected start")
    {
        ResponsePayload::SessionRuntime {
            runtime_id: None, ..
        } => {}
        other => panic!("rejected start must close the daemon runtime, got {other:?}"),
    }

    client.shutdown();
    shutdown.store(true, Ordering::Release);
    std::fs::remove_dir_all(directory).ok();
}
