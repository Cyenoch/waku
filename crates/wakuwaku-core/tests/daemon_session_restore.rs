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
use wakuwaku_core::auth::{
    AuthRuntime, AuthService, CredentialStore, MemoryCredentialStore, StoredCredential,
};
use wakuwaku_core::daemon::WakuBackend;
use wakuwaku_core::model::{
    DriverEvent, InteractionMode, MessageRole, ProviderId, RuntimeMode, TurnStatus,
};
use wakuwaku_core::persistence::{PersistedState, StateStore};
use wakuwaku_core::protocol::{Command, ResponsePayload, StartTask};
use wakuwaku_core::{DaemonSettings, DaemonSettingsStore, ServerOptions, serve};
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

/// Mock provider whose response bodies are held until a test releases them,
/// reproducing an in-flight provider request at daemon-crash time.
struct GatedMockHttp {
    responses: Mutex<VecDeque<String>>,
    arrived: Mutex<Vec<std::path::PathBuf>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    released: std::sync::mpsc::Sender<()>,
}

impl GatedMockHttp {
    fn new(bodies: Vec<String>) -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        Arc::new(Self {
            responses: Mutex::new(bodies.into()),
            arrived: Mutex::new(Vec::new()),
            release: Mutex::new(rx),
            released: tx,
        })
    }

    fn bind(self: &Arc<Self>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Arc::clone(self);
        thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mock = Arc::clone(&mock);
                        thread::spawn(move || serve_gated_http(stream, &mock));
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

    fn wait_arrival(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !self.arrived.lock().is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("provider request never arrived");
    }

    fn release(&self) {
        let _ = self.released.send(());
    }
}

fn serve_gated_http(mut stream: TcpStream, mock: &GatedMockHttp) {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
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
    // Record that the request arrived, then hold the response until released.
    let request = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let target = request
        .lines()
        .find_map(|line| line.split_once(' ').map(|(_, path)| path.to_owned()))
        .unwrap_or_default();
    mock.arrived
        .lock()
        .push(std::path::PathBuf::from(format!("POST {target}")));
    let body = mock.responses.lock().pop_front().unwrap_or_default();
    let _ = mock.release.lock().recv_timeout(Duration::from_secs(20));
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

fn gated_daemon(
    directory: &std::path::Path,
    workspace: &std::path::Path,
) -> (DaemonClient, Uuid, Arc<GatedMockHttp>, Arc<AtomicBool>) {
    let mock = GatedMockHttp::new(vec![model_sse("held")]);
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
        directory,
        creds,
        AuthEndpoints::production(),
    ))
    .unwrap();
    let store = StateStore::daemon(directory.join("app.db"));
    let mut state = PersistedState::fresh(workspace.to_path_buf());
    let session_id = state.sessions[0].id;
    state.sessions[0].provider = ProviderId::new(PROVIDER);
    state.sessions[0].model = Some("restore-model".into());
    state.sessions[0].begin_turn("seed");
    store.save(&mut state).unwrap();
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
    (client, session_id, mock, shutdown)
}

#[test]
fn prompt_admission_snapshot_is_persisted_before_provider_dispatch() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-admit-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, session_id, mock, shutdown) = gated_daemon(&directory, &workspace);
    let session = {
        let store = StateStore::daemon(directory.join("app.db"));
        store
            .load()
            .unwrap()
            .sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .unwrap()
    };
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
    handle.prompt(PromptInput::text("held prompt"));

    // The provider request arrives and is held; the admitted snapshot must
    // already be durable before the response completes.
    mock.wait_arrival();
    let store = StateStore::daemon(directory.join("app.db"));
    let snapshot = store
        .read_snapshot_file_only(session_id)
        .unwrap()
        .expect("admitted-prompt snapshot must exist while the provider request is in flight");
    let user_texts: Vec<String> = snapshot
        .transcript()
        .iter()
        .filter_map(|message| match message {
            wakuwaku_harness::Message::User(user) => Some(
                user.parts
                    .iter()
                    .filter_map(|part| match part {
                        wakuwaku_harness::UserPart::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_texts
            .iter()
            .filter(|text| text.as_str() == "held prompt")
            .count(),
        1,
        "admitted prompt must be in the durable snapshot exactly once"
    );

    mock.release();
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::TurnFinished { success: true, .. })
    });
    drop(handle);
    client.shutdown();
    shutdown.store(true, Ordering::Release);
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn failed_admission_persistence_rolls_back_prompt_and_skips_provider() {
    let directory =
        std::env::temp_dir().join(format!("wakuwaku-admit-rollback-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, session_id, mock, shutdown) = gated_daemon(&directory, &workspace);
    let session = {
        let store = StateStore::daemon(directory.join("app.db"));
        store
            .load()
            .unwrap()
            .sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .unwrap()
    };
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

    // Make the atomic snapshot write fail: `File::create` on a path whose
    // parent is a directory errors, so the tmp path becomes a directory.
    let store = StateStore::daemon(directory.join("app.db"));
    store.read_snapshot_file_only(session_id).unwrap();
    let snapshots = directory.join("snapshots");
    std::fs::create_dir_all(snapshots.join(format!("{session_id}.json.tmp"))).unwrap();

    handle.prompt(PromptInput::text("B"));

    let mut saw_error = false;
    let mut saw_failed_finish = false;
    let mut saw_turn_started = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = rx.recv_timeout(remaining) else {
            break;
        };
        match event {
            DriverEvent::TurnStarted => saw_turn_started = true,
            DriverEvent::Error(message) => {
                assert!(
                    message.contains("could not persist the admitted prompt"),
                    "{message}"
                );
                saw_error = true;
            }
            DriverEvent::TurnFinished { success: false, .. } => saw_failed_finish = true,
            _ => {}
        }
        if saw_error && saw_failed_finish {
            break;
        }
    }
    assert!(saw_error, "persistence failure must surface as an error");
    assert!(saw_failed_finish, "turn must finish as failed");
    assert!(
        !saw_turn_started,
        "TurnStarted must never precede a failed write"
    );
    assert!(
        mock.arrived.lock().is_empty(),
        "provider must not be dispatched for an unpersisted prompt"
    );

    // Rollback: removing the blocker lets the next prompt through and the
    // provider context contains only that prompt, proving B was rolled back.
    std::fs::remove_dir_all(snapshots.join(format!("{session_id}.json.tmp"))).unwrap();
    handle.prompt(PromptInput::text("C"));
    mock.wait_arrival();
    mock.release();
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::TurnFinished { success: true, .. })
    });
    let snapshot = store
        .read_snapshot_file_only(session_id)
        .unwrap()
        .expect("snapshot after C");
    let user_texts: Vec<String> = snapshot
        .transcript()
        .iter()
        .filter_map(|message| match message {
            wakuwaku_harness::Message::User(user) => Some(
                user.parts
                    .iter()
                    .filter_map(|part| match part {
                        wakuwaku_harness::UserPart::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect();
    assert!(
        !user_texts.iter().any(|text| text.as_str() == "B"),
        "rolled-back prompt must not survive: {user_texts:?}"
    );
    assert_eq!(
        user_texts
            .iter()
            .filter(|text| text.as_str() == "C")
            .count(),
        1
    );

    drop(handle);
    client.shutdown();
    shutdown.store(true, Ordering::Release);
    std::fs::remove_dir_all(directory).ok();
}

fn gated_daemon_with_event_log(
    directory: &std::path::Path,
    workspace: &std::path::Path,
    session_event_log: bool,
) -> (DaemonClient, Uuid, Arc<GatedMockHttp>, Arc<AtomicBool>) {
    let mock = GatedMockHttp::new(vec![model_sse("logged")]);
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
        directory,
        creds,
        AuthEndpoints::production(),
    ))
    .unwrap();
    let store = StateStore::daemon(directory.join("app.db"));
    let mut state = PersistedState::fresh(workspace.to_path_buf());
    let session_id = state.sessions[0].id;
    state.sessions[0].provider = ProviderId::new(PROVIDER);
    state.sessions[0].model = Some("restore-model".into());
    // Mirror the app: it begins the turn client-side before sending Prompt,
    // so the daemon projection carries exactly one Running turn.
    state.sessions[0].begin_turn("logged prompt");
    store.save(&mut state).unwrap();
    let backend =
        WakuBackend::new_with_auth_and_session_event_log(settings, store, auth, session_event_log)
            .unwrap();
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
    (client, session_id, mock, shutdown)
}

fn shadow_event_rows(
    directory: &std::path::Path,
    session_id: Uuid,
) -> Vec<(i64, String, String, String)> {
    let connection = rusqlite::Connection::open(directory.join("app.db")).unwrap();
    connection
        .prepare(
            "SELECT seq, kind, payload_json, event_id FROM session_events
             WHERE stream_id = ?1 ORDER BY seq",
        )
        .unwrap()
        .query_map([session_id.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
}

#[test]
fn session_event_log_captures_prompt_turn_usage_finish_when_enabled() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-shadow-on-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, session_id, mock, shutdown) =
        gated_daemon_with_event_log(&directory, &workspace, true);
    let mut session = {
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = store.load().unwrap();
        let mut session = state
            .sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .unwrap();
        store.hydrate(&mut session).unwrap();
        session
    };
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
    handle.prompt(PromptInput::text("logged prompt"));
    mock.wait_arrival();
    mock.release();
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::TurnFinished { success: true, .. })
    });

    let rows = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let rows = shadow_event_rows(&directory, session_id);
            if rows
                .iter()
                .any(|(seq, kind, _, _)| kind == "turn_finished" && *seq > 0)
            {
                break rows;
            }
            if Instant::now() > deadline {
                panic!("shadow rows never flushed: {rows:?}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    };
    let kinds: Vec<&str> = rows.iter().map(|(_, kind, _, _)| kind.as_str()).collect();
    let prompt_at = kinds
        .iter()
        .position(|kind| *kind == "prompt_observed")
        .unwrap_or_else(|| panic!("prompt_observed recorded: {kinds:?}"));
    let started_at = kinds
        .iter()
        .position(|kind| *kind == "turn_started")
        .expect("turn_started recorded");
    let usage_at = kinds
        .iter()
        .position(|kind| *kind == "usage_recorded")
        .expect("usage_recorded recorded");
    let finished_at = kinds
        .iter()
        .position(|kind| *kind == "turn_finished")
        .expect("turn_finished recorded");
    assert!(prompt_at < started_at, "{kinds:?}");
    assert!(started_at < usage_at, "{kinds:?}");
    assert!(usage_at < finished_at, "{kinds:?}");
    assert!(
        kinds.iter().all(|kind| !kind.contains("delta")),
        "deltas stay live-only: {kinds:?}"
    );
    // Contiguous seq from 1.
    for (index, (seq, _, _, _)) in rows.iter().enumerate() {
        assert_eq!(*seq, (index + 1) as i64, "{kinds:?}");
    }
    // The usage event id matches the legacy usage_events row.
    let usage_payload = &rows[usage_at].2;
    let usage: serde_json::Value = serde_json::from_str(usage_payload).unwrap();
    let shadow_id = usage["usage_event_id"].as_str().unwrap();
    let envelope_id = &rows[usage_at].3;
    assert_eq!(
        envelope_id, shadow_id,
        "the usage envelope event id must reuse the legacy usage event id"
    );
    let legacy = {
        let store = StateStore::daemon(directory.join("app.db"));
        store
            .usage_events_between(0, i64::MAX)
            .unwrap()
            .into_iter()
            .map(|event| event.event_id.to_string())
            .collect::<Vec<_>>()
    };
    assert!(legacy.contains(&shadow_id.to_owned()), "{legacy:?}");

    drop(handle);
    client.shutdown();
    shutdown.store(true, Ordering::Release);
    std::fs::remove_dir_all(directory).ok();
}

#[test]
fn session_event_log_writes_nothing_when_disabled() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-shadow-off-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, session_id, mock, shutdown) =
        gated_daemon_with_event_log(&directory, &workspace, false);
    let session = {
        let store = StateStore::daemon(directory.join("app.db"));
        store
            .load()
            .unwrap()
            .sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .unwrap()
    };
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
    handle.prompt(PromptInput::text("quiet prompt"));
    mock.wait_arrival();
    mock.release();
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::TurnFinished { success: true, .. })
    });
    thread::sleep(Duration::from_millis(200));
    assert!(shadow_event_rows(&directory, session_id).is_empty());

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
