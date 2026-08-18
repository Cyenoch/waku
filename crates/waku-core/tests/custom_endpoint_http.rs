//! Custom endpoint discovery and native-adapter request routing.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::json;
use uuid::Uuid;
use waku_client::driver::{DriverHandle, DriverStartOptions, event_channel};
use waku_client::{DaemonClient, PromptInput};
use waku_core::auth::{AuthRuntime, AuthService, MemoryCredentialStore};
use waku_core::daemon::WakuBackend;
use waku_core::persistence::{PersistedState, StateStore};
use waku_core::{DaemonSettingsStore, ServerOptions, serve};
use waku_protocol::AuthEndpoints;
use waku_protocol::model::{InteractionMode, RuntimeMode};
use waku_protocol::{
    ApiFormat, Command, DaemonSettings, ExternalProvider, ProviderId, ResponsePayload,
    UnsupportedReason,
};

const TOKEN: &str = "custom-endpoint-token";

type MockRoutes = Vec<(String, VecDeque<(u16, String)>)>;

struct Recorded {
    method: String,
    path: String,
    body: String,
    authorization: Option<String>,
    api_key: Option<String>,
}

struct MockHttp {
    recorded: Mutex<Vec<Recorded>>,
    routes: Mutex<MockRoutes>,
}

impl MockHttp {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            recorded: Mutex::new(Vec::new()),
            routes: Mutex::new(Vec::new()),
        })
    }

    fn push(&self, path: &str, status: u16, body: impl Into<String>) {
        let mut routes = self.routes.lock();
        if let Some((_, queue)) = routes.iter_mut().find(|(prefix, _)| prefix == path) {
            queue.push_back((status, body.into()));
            return;
        }
        let mut queue = VecDeque::new();
        queue.push_back((status, body.into()));
        routes.push((path.to_owned(), queue));
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.recorded
            .lock()
            .iter()
            .map(|row| Recorded {
                method: row.method.clone(),
                path: row.path.clone(),
                body: row.body.clone(),
                authorization: row.authorization.clone(),
                api_key: row.api_key.clone(),
            })
            .collect()
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
    let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let request_line = headers.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
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
    let body = String::from_utf8_lossy(&buf[header_end..]).into_owned();
    let header = |name: &str| {
        headers.lines().find_map(|line| {
            line.split_once(':').and_then(|(key, value)| {
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        })
    };
    mock.recorded.lock().push(Recorded {
        method,
        path: path.clone(),
        body,
        authorization: header("authorization"),
        api_key: header("x-api-key"),
    });
    let next = {
        let mut routes = mock.routes.lock();
        routes
            .iter_mut()
            .find(|(prefix, _)| path.starts_with(prefix.as_str()))
            .and_then(|(_, queue)| queue.pop_front())
    };
    let (status, payload) = next.unwrap_or((404, "{}".into()));
    let response = format!(
        "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn sse_responses(text: &str) -> String {
    [
        json!({"type":"response.created","response":{"id":"resp"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":text}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"m","content":[{"type":"output_text","text":text}]}}),
        json!({"type":"response.completed","response":{"id":"resp","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\r\n\r\n"))
    .collect()
}

fn sse_chat(text: &str) -> String {
    [
        json!({"id":"c","choices":[{"delta":{"content":text}}]}),
        json!({"id":"c","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\r\n\r\n"))
    .collect()
}

fn sse_anthropic(text: &str) -> String {
    [
        json!({"type":"message_start","message":{"id":"msg","usage":{"input_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\r\n\r\n"))
    .collect()
}

struct Harness {
    client: DaemonClient,
    workspace: std::path::PathBuf,
    session_id: Uuid,
    shutdown: Arc<AtomicBool>,
}

fn start_daemon(provider: Option<&str>, model: Option<&str>) -> Harness {
    let directory = std::env::temp_dir().join(format!("waku-custom-ep-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
    let store = StateStore::daemon(directory.join("app.db"));
    let mut state = PersistedState::fresh(workspace.clone());
    if let Some(provider) = provider {
        state.sessions[0].provider = ProviderId::new(provider);
    }
    if let Some(model) = model {
        state.sessions[0].model = Some(model.to_owned());
    }
    state.sessions[0].begin_turn("custom-endpoint");
    let session_id = state.sessions[0].id;
    state.mark_session_dirty(session_id);
    store.save(&mut state).unwrap();
    let auth = AuthService::new(AuthRuntime::testing(
        &directory,
        Arc::new(MemoryCredentialStore::default()),
        AuthEndpoints::production(),
    ))
    .unwrap();
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
    Harness {
        client,
        workspace,
        session_id,
        shutdown,
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn openai_models_body() -> String {
    json!({
        "data": [
            { "id": "gpt-5", "name": "GPT-5" },
            { "id": "text-embedding-3-small" },
            { "id": "whisper-1" }
        ]
    })
    .to_string()
}

fn anthropic_models_body() -> String {
    json!({
        "data": [
            { "id": "claude-sonnet-4-5", "display_name": "Sonnet" }
        ]
    })
    .to_string()
}

#[test]
fn update_settings_rejects_builtin_id_and_unusable_url() {
    let harness = start_daemon(None, None);
    let reserved = DaemonSettings {
        external_providers: vec![ExternalProvider::new(
            ProviderId::OPENAI_RESPONSES,
            "Looks configurable",
            "http://127.0.0.1:9/v1",
            ApiFormat::OpenAiResponses,
            "gpt-5",
        )],
        extra: Default::default(),
    };
    let error = harness
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::UpdateSettings { settings: reserved },
        )
        .unwrap_err();
    assert!(error.to_string().contains("reserved"), "{error}");

    let invalid = DaemonSettings {
        external_providers: vec![ExternalProvider::new(
            "corp-broken",
            "Broken",
            "ftp://example.test/v1",
            ApiFormat::OpenAiChat,
            "gpt-5",
        )],
        extra: Default::default(),
    };
    assert!(
        harness
            .client
            .request(
                Uuid::nil(),
                Uuid::nil(),
                Command::UpdateSettings { settings: invalid },
            )
            .is_err()
    );
}

#[test]
fn custom_responses_discovers_and_routes_selected_model() {
    run_custom_format(CustomCase {
        id: "corp-responses",
        env: "WAKU_CUSTOM_RESP_KEY",
        key: "sk-resp",
        format: ApiFormat::OpenAiResponses,
        models: openai_models_body(),
        completion_path: "/v1/responses",
        completion: sse_responses("ok"),
        select: "gpt-5",
        unsupported: Some("text-embedding-3-small"),
        bearer: true,
    });
}

#[test]
fn custom_chat_discovers_and_routes_selected_model() {
    run_custom_format(CustomCase {
        id: "corp-chat",
        env: "WAKU_CUSTOM_CHAT_KEY",
        key: "sk-chat",
        format: ApiFormat::OpenAiChat,
        models: openai_models_body(),
        completion_path: "/v1/chat/completions",
        completion: sse_chat("ok"),
        select: "gpt-5",
        unsupported: Some("whisper-1"),
        bearer: true,
    });
}

#[test]
fn custom_anthropic_discovers_and_routes_selected_model() {
    run_custom_format(CustomCase {
        id: "corp-anthropic",
        env: "WAKU_CUSTOM_ANTH_KEY",
        key: "sk-ant",
        format: ApiFormat::Anthropic,
        models: anthropic_models_body(),
        completion_path: "/v1/messages",
        completion: sse_anthropic("ok"),
        select: "claude-sonnet-4-5",
        unsupported: None,
        bearer: false,
    });
}

struct CustomCase {
    id: &'static str,
    env: &'static str,
    key: &'static str,
    format: ApiFormat,
    models: String,
    completion_path: &'static str,
    completion: String,
    select: &'static str,
    unsupported: Option<&'static str>,
    bearer: bool,
}

fn run_custom_format(case: CustomCase) {
    unsafe {
        std::env::set_var(case.env, case.key);
    }
    let mock = MockHttp::new();
    mock.push("/v1/models", 200, case.models);
    mock.push(case.completion_path, 200, case.completion);
    let port = mock.bind();
    let base = format!("http://127.0.0.1:{port}/v1");
    let harness = start_daemon(Some(case.id), Some(case.select));
    let mut provider =
        ExternalProvider::new(case.id, case.id, base.clone(), case.format, case.select);
    provider.api_key_env = Some(case.env.into());
    harness
        .client
        .request(
            Uuid::nil(),
            Uuid::nil(),
            Command::UpdateSettings {
                settings: DaemonSettings {
                    external_providers: vec![provider],
                    extra: Default::default(),
                },
            },
        )
        .unwrap();

    let ResponsePayload::Models { catalog } = harness
        .client
        .refresh_models(ProviderId::new(case.id))
        .unwrap()
    else {
        panic!("models");
    };
    let discovery = mock
        .recorded()
        .into_iter()
        .find(|row| row.path.starts_with("/v1/models"))
        .expect("GET /models");
    assert_eq!(discovery.method, "GET");
    if case.bearer {
        assert_eq!(
            discovery.authorization.as_deref(),
            Some(format!("Bearer {}", case.key).as_str())
        );
    } else {
        assert_eq!(discovery.api_key.as_deref(), Some(case.key));
    }
    let selected = catalog
        .models
        .iter()
        .find(|entry| entry.id == case.select)
        .expect("selected model in catalog");
    assert!(selected.supported);
    assert_eq!(selected.api_format, case.format);
    assert_eq!(selected.base_url, base);
    if let Some(unsupported) = case.unsupported {
        let hidden = catalog
            .models
            .iter()
            .find(|entry| entry.id == unsupported)
            .expect("non-chat remains visible");
        assert!(!hidden.supported);
        assert_eq!(hidden.unsupported_reason, Some(UnsupportedReason::NonChat));
    }

    if let Some(unsupported) = case.unsupported {
        let (wake, _) = smol::channel::bounded(1);
        let (events, _rx) = event_channel(wake);
        let started = DriverHandle::start(
            harness.client.clone(),
            harness.session_id,
            DriverStartOptions {
                provider: ProviderId::new(case.id),
                cwd: harness.workspace.clone(),
                mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Build,
                model: Some(unsupported.into()),
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
            },
            events,
        );
        assert!(started.is_err(), "unsupported model must not start");
        assert!(!mock.recorded().iter().any(|row| {
            row.path
                .contains(case.completion_path.trim_start_matches("/v1"))
        }));
    }

    let (wake, _) = smol::channel::bounded(1);
    let (events, rx) = event_channel(wake);
    let handle = DriverHandle::start(
        harness.client.clone(),
        harness.session_id,
        DriverStartOptions {
            provider: ProviderId::new(case.id),
            cwd: harness.workspace.clone(),
            mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Build,
            model: Some(case.select.into()),
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
        },
        events,
    )
    .unwrap();
    wait_event(&rx, |event| {
        matches!(event, waku_protocol::model::DriverEvent::Connected)
    });
    handle.prompt(PromptInput::text("hello"));
    wait_event(&rx, |event| {
        matches!(
            event,
            waku_protocol::model::DriverEvent::TurnFinished { .. }
        )
    });
    let completion = mock
        .recorded()
        .into_iter()
        .find(|row| {
            row.path
                .contains(case.completion_path.trim_start_matches("/v1"))
        })
        .unwrap_or_else(|| panic!("missing {}", case.completion_path));
    assert!(
        completion.path.contains(case.completion_path),
        "{}",
        completion.path
    );
    assert!(
        completion
            .body
            .contains(&format!("\"model\":\"{}\"", case.select)),
        "{}",
        completion.body
    );
    if case.bearer {
        assert_eq!(
            completion.authorization.as_deref(),
            Some(format!("Bearer {}", case.key).as_str())
        );
    } else {
        assert_eq!(completion.api_key.as_deref(), Some(case.key));
    }
}

fn wait_event(
    rx: &crossbeam_channel::Receiver<waku_protocol::model::DriverEvent>,
    mut pred: impl FnMut(&waku_protocol::model::DriverEvent) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = rx
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("timed out waiting for driver event"));
        if pred(&event) {
            return;
        }
    }
}
