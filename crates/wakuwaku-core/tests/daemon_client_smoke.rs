//! Production daemon/client path against an in-process OpenAI mock.

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
use wakuwaku_core::driver::WAKU_SYSTEM_PROMPT;
use wakuwaku_core::model::{DriverEvent, InteractionMode, ProviderId, RuntimeMode};
use wakuwaku_core::persistence::{PersistedState, StateStore};
use wakuwaku_core::protocol::{Command, ResponsePayload};
use wakuwaku_core::{DaemonSettings, DaemonSettingsStore, ServerOptions, serve};
use wakuwaku_protocol::{ApiFormat, ExternalProvider, PromptAttachmentSource, PromptImageRef};

const TOKEN: &str = "smoke-token";
const PROVIDER: &str = "smoke-openai";

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect()
}

struct MockHttp {
    responses: Mutex<VecDeque<MockResponse>>,
    requests: Mutex<Vec<Value>>,
}

struct MockResponse {
    body: String,
    delay: Duration,
}

impl MockHttp {
    fn new(bodies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(
                bodies
                    .into_iter()
                    .map(|body| MockResponse {
                        body,
                        delay: Duration::ZERO,
                    })
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn push_delayed(&self, body: String, delay: Duration) {
        self.responses
            .lock()
            .push_back(MockResponse { body, delay });
    }

    fn bind(self: &Arc<Self>) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mock = Arc::clone(self);
        let handle = thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
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
        (port, handle)
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
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
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
    if let Ok(body) = serde_json::from_slice::<Value>(&buf[header_end..]) {
        mock.requests.lock().push(body);
    }
    let next = mock.responses.lock().pop_front();
    let Some(next) = next else {
        return;
    };
    if !next.delay.is_zero() {
        thread::sleep(next.delay);
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        next.body.len(),
        next.body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn first_model_sse() -> String {
    sse(&[
        json!({"type":"response.created","response":{"id":"resp_1"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}),
        json!({"type":"response.reasoning_text.delta","output_index":0,"delta":"plan the call"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}),
        json!({"type":"response.output_text.delta","output_index":1,"delta":"calling"}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"calling"}]}}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call1","id":"fc1","name":"shell"}}),
        json!({"type":"response.function_call_arguments.done","output_index":2,"arguments":"{\"command\":\"echo smoke\"}"}),
        json!({"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"call1","id":"fc1","name":"shell","arguments":"{\"command\":\"echo smoke\"}"}}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":11,"output_tokens":6,"total_tokens":17}}}),
    ])
}

fn second_model_sse() -> String {
    sse(&[
        json!({"type":"response.created","response":{"id":"resp_2"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_2"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"hello"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_2","content":[{"type":"output_text","text":"hello world"}]}}),
        json!({"type":"response.completed","response":{"id":"resp_2","status":"completed","usage":{"input_tokens":20,"output_tokens":4,"total_tokens":24}}}),
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

#[test]
fn daemon_client_mock_http_smoke() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-smoke-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let mock = MockHttp::new(vec![first_model_sse(), second_model_sse()]);
    mock.push_delayed(second_model_sse(), Duration::from_secs(20));
    let (port, _http) = mock.bind();
    unsafe {
        std::env::set_var("WAKUWAKU_SMOKE_KEY", "sk-smoke");
    }

    let mut provider = ExternalProvider::new(
        PROVIDER,
        "Smoke OpenAI",
        format!("http://127.0.0.1:{port}/v1"),
        ApiFormat::OpenAiResponses,
        "smoke-model",
    );
    provider.api_key_env = Some("WAKUWAKU_SMOKE_KEY".into());

    let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
    settings
        .replace(DaemonSettings {
            external_providers: vec![provider],
            extra: Default::default(),
        })
        .unwrap();

    let store = StateStore::daemon(directory.join("app.db"));
    let mut state = PersistedState::fresh(workspace.clone());
    state.sessions[0].provider = ProviderId::new(PROVIDER);
    state.sessions[0].begin_turn("inspect the image");
    let session_id = state.sessions[0].id;
    state.mark_session_dirty(session_id);
    store.save(&mut state).unwrap();

    let backend = WakuBackend::new(settings, store).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = shutdown.clone();
    let server = thread::spawn(move || {
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
    let handle = DriverHandle::start(
        client.clone(),
        session_id,
        DriverStartOptions {
            provider: ProviderId::new(PROVIDER),
            cwd: workspace.clone(),
            mode: RuntimeMode::Ask,
            interaction_mode: InteractionMode::Build,
            model: Some("smoke-model".into()),
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
        },
        events,
    )
    .unwrap();
    assert!(handle.supports_steer());
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::Connected)
    });

    let attacher = DaemonClient::connect(&address.to_string(), TOKEN.into()).unwrap();
    let runtime_id = client
        .last_sequences()
        .into_iter()
        .find(|cursor| cursor.session_id == session_id)
        .map(|cursor| cursor.runtime_id)
        .expect("started runtime");
    let attached = attacher.subscribe(session_id, runtime_id);
    let _replayed = attached.recv_timeout(Duration::from_secs(2));

    let stored = match client
        .request(
            session_id,
            Uuid::nil(),
            Command::StoreBlob {
                mime_type: "image/png".into(),
                bytes: tiny_png().to_vec(),
            },
        )
        .unwrap()
    {
        ResponsePayload::BlobStored { reference, .. } => reference,
        other => panic!("unexpected blob response {other:?}"),
    };

    handle.prompt(PromptInput {
        text: "inspect the image".into(),
        display_text: None,
        attachments: vec![PromptImageRef::Blob {
            reference: stored.clone(),
        }],
        sources: vec![PromptAttachmentSource::from_named_attachment(
            Some(stored),
            "image.png",
            "image.png",
            false,
            true,
        )],
    });

    wait_event(
        &rx,
        Duration::from_secs(5),
        |event| matches!(event, DriverEvent::ReasoningDelta(delta) if delta.contains("plan")),
    );
    wait_event(
        &rx,
        Duration::from_secs(5),
        |event| matches!(event, DriverEvent::TextDelta(delta) if delta.contains("calling")),
    );
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::UsageUpdated { .. })
    });
    let permission = wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::Permission { .. })
    });
    let DriverEvent::Permission { request_id, .. } = permission else {
        panic!("expected permission");
    };
    handle.steer(PromptInput::text("prefer echo"));
    handle.respond(request_id, "allow-session".into());

    wait_event(
        &rx,
        Duration::from_secs(8),
        |event| matches!(event, DriverEvent::TextDelta(delta) if delta.contains("hello")),
    );
    wait_event(&rx, Duration::from_secs(8), |event| {
        matches!(event, DriverEvent::TurnFinished { success: true, .. })
    });

    let forked = match client
        .request(
            session_id,
            Uuid::nil(),
            Command::ForkSessionFromResponse { turn_count: 1 },
        )
        .unwrap()
    {
        ResponsePayload::SessionForked { session, .. } => session.id,
        other => panic!("unexpected fork response {other:?}"),
    };
    assert_ne!(forked, session_id);

    handle.prompt(PromptInput::text("hang"));
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(event, DriverEvent::TurnStarted)
    });
    handle.cancel();
    wait_event(&rx, Duration::from_secs(5), |event| {
        matches!(
            event,
            DriverEvent::TurnFinished { success: false, .. } | DriverEvent::Error(_)
        )
    });

    handle.apply_options(wakuwaku_client::driver::SessionOptions {
        mode: RuntimeMode::Ask,
        interaction_mode: InteractionMode::Build,
        model: Some("smoke-model".into()),
        reasoning_effort: None,
        service_tier: None,
        context_window: None,
    });
    drop(handle);
    client.shutdown();
    shutdown.store(true, Ordering::Release);
    let _ = server.join();

    let restored = StateStore::daemon(directory.join("app.db"));
    let mut state = restored.load().unwrap();
    restored.hydrate(&mut state.sessions[0]).unwrap();
    let snapshot = restored
        .load_harness_snapshot(session_id)
        .unwrap()
        .expect("persisted harness snapshot");
    assert_eq!(snapshot.system_prompt.as_deref(), Some(WAKU_SYSTEM_PROMPT));
    assert!(
        snapshot
            .messages
            .iter()
            .any(|message| matches!(message, wakuwaku_harness::Message::User(_))),
        "user prompt should persist"
    );
    assert!(
        mock.requests.lock().len() >= 2,
        "expected model then tool follow-up"
    );
    let first = &mock.requests.lock()[0];
    assert!(
        first.to_string().contains("inspect the image")
            || first.to_string().contains("image/png")
            || first["input"].to_string().contains("inspect"),
        "first request should carry the user prompt"
    );

    std::fs::remove_dir_all(directory).ok();
}

fn tiny_png() -> &'static [u8] {
    &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, b'I', b'D', b'A', b'T', 0x78, 0x9c, 0x63, 0xf8,
        0x0f, 0x00, 0x00, 0x01, 0x01, 0x00, 0x05, 0x18, 0xd8, 0x4e, 0x00, 0x00, 0x00, 0x00, b'I',
        b'E', b'N', b'D', 0xae, b'B', b'`', 0x82,
    ]
}
