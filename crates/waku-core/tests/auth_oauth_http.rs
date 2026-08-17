//! Local HTTP fixtures for Codex/xAI OAuth and dual-format Apply.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::json;
use uuid::Uuid;
use waku_client::driver::{DriverHandle, DriverStartOptions, event_channel};
use waku_client::{DaemonClient, PromptInput};
use waku_core::auth::{
    AuthRuntime, AuthService, CredentialStore, MemoryCredentialStore, StoredCredential,
};
use waku_core::daemon::WakuBackend;
use waku_core::persistence::{PersistedState, StateStore};
use waku_core::{DaemonSettingsStore, ServerOptions, serve};
use waku_protocol::model::{InteractionMode, RuntimeMode};
use waku_protocol::{
    AuthEndpoints, AuthMethod, AuthPhase, CatalogSource, LoginMethod, ModelCapabilities,
    ModelCatalogEntry, ProviderId, ProviderPreset, ResponsePayload, SecretString, TransportProfile,
    xai_oauth_seed,
};
use waku_provider::{apply_opencode_route, apply_xai_policy};

const TOKEN: &str = "oauth-http-token";

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

fn wait_login_settled(service: &AuthService, login_id: Uuid) -> AuthPhase {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let phase = service.poll_login(login_id).unwrap();
        if matches!(
            phase,
            AuthPhase::Completed { .. } | AuthPhase::Failed { .. }
        ) || Instant::now() >= deadline
        {
            return phase;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn serve_http(mut stream: TcpStream, mock: &MockHttp) {
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

fn chatgpt_jwt(account: &str) -> String {
    let payload =
        format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account}"}}}}"#);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
    format!("hdr.{body}.sig")
}

fn token_json(access: &str, refresh: &str, expires_in: u64) -> String {
    json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": expires_in,
        "id_token": chatgpt_jwt("acct_mail"),
    })
    .to_string()
}

fn injected_codex_endpoints(port: u16) -> AuthEndpoints {
    let origin = format!("http://127.0.0.1:{port}");
    AuthEndpoints {
        openai_authorize: format!("{origin}/oauth/authorize"),
        openai_token: format!("{origin}/oauth/token"),
        openai_device_usercode: format!("{origin}/device/usercode"),
        openai_device_token: format!("{origin}/device/token"),
        openai_device_auth_url: format!("{origin}/codex/device"),
        openai_device_redirect: format!("{origin}/deviceauth/callback"),
        xai_discovery: format!("{origin}/.well-known/openid-configuration"),
        xai_device_code: format!("{origin}/oauth2/device/code"),
        xai_userinfo: format!("{origin}/oauth2/userinfo"),
        xai_allowed_token_hosts: vec!["127.0.0.1".into()],
    }
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

fn catalog_stub(provider: &str, id: &str, name: &str, base_url: &str) -> ModelCatalogEntry {
    ModelCatalogEntry {
        provider: ProviderId::new(provider),
        id: id.into(),
        name: name.into(),
        api_format: waku_protocol::ApiFormat::OpenAiResponses,
        transport: TransportProfile::Standard,
        base_url: base_url.into(),
        context_window: 128_000,
        max_output_tokens: 8_192,
        reasoning: false,
        capabilities: ModelCapabilities::openai_compatible(
            waku_protocol::ApiFormat::OpenAiResponses,
        ),
        supported: true,
        unsupported_reason: None,
    }
}

fn opencode_entry(
    preset: ProviderPreset,
    id: &str,
    name: &str,
    base_url: &str,
) -> ModelCatalogEntry {
    apply_opencode_route(catalog_stub(preset.id(), id, name, base_url), preset)
}

fn assert_routed_request(row: &Recorded, path_part: &str, model: &str, tier: Option<&str>) {
    assert!(
        row.path.contains(path_part),
        "path {} did not contain {path_part}",
        row.path
    );
    assert!(
        row.body.contains(&format!("\"model\":\"{model}\"")),
        "body missing model {model}: {}",
        row.body
    );
    match tier {
        Some(tier) => assert!(
            row.body.contains(&format!("\"service_tier\":\"{tier}\"")),
            "body missing service_tier {tier}: {}",
            row.body
        ),
        None => assert!(
            !row.body.contains("service_tier"),
            "service_tier must be omitted: {}",
            row.body
        ),
    }
}

struct LiveSession {
    mock: Arc<MockHttp>,
    handle: DriverHandle,
    rx: crossbeam_channel::Receiver<waku_protocol::model::DriverEvent>,
    shutdown: Arc<AtomicBool>,
}

impl LiveSession {
    fn start(
        label: &str,
        provider: &str,
        model: &str,
        cred: StoredCredential,
        service_tier: Option<waku_protocol::ServiceTier>,
        routes: &[(&str, String)],
        catalog: impl FnOnce(&str) -> Vec<ModelCatalogEntry>,
    ) -> Self {
        let mock = MockHttp::new();
        for (path, body) in routes {
            mock.push(path, 200, body.clone());
        }
        let port = mock.bind();
        let base = format!("http://127.0.0.1:{port}/v1");
        let directory = std::env::temp_dir().join(format!("waku-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let workspace = directory.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let creds = Arc::new(MemoryCredentialStore::default());
        creds.set(&ProviderId::new(provider), cred).unwrap();
        AuthRuntime::testing(
            &directory,
            Arc::clone(&creds) as Arc<dyn CredentialStore>,
            AuthEndpoints::production(),
        )
        .persist
        .put_catalog(
            &ProviderId::new(provider),
            catalog(&base),
            CatalogSource::Live,
            1,
        )
        .unwrap();

        let state_store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(workspace.clone());
        state.sessions[0].provider = ProviderId::new(provider);
        state.sessions[0].model = Some(model.to_owned());
        state.sessions[0].begin_turn(label);
        let session_id = state.sessions[0].id;
        state.mark_session_dirty(session_id);
        state_store.save(&mut state).unwrap();
        let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
        let runtime = AuthRuntime::testing(
            &directory,
            Arc::clone(&creds) as Arc<dyn CredentialStore>,
            AuthEndpoints::production(),
        );
        let backend =
            WakuBackend::new_with_auth(settings, state_store, AuthService::new(runtime).unwrap())
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
        let (wake, _) = smol::channel::bounded(1);
        let (events, rx) = event_channel(wake);
        let handle = DriverHandle::start(
            client,
            session_id,
            DriverStartOptions {
                provider: ProviderId::new(provider),
                cwd: workspace,
                mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Build,
                model: Some(model.to_owned()),
                reasoning_effort: None,
                service_tier,
                context_window: None,
            },
            events,
        )
        .unwrap();
        wait_connected(&rx);
        Self {
            mock,
            handle,
            rx,
            shutdown,
        }
    }

    fn prompt(&self, text: &str) {
        self.handle.prompt(PromptInput::text(text));
        wait_finished(&self.rx);
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.mock.recorded()
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

#[test]
fn codex_browser_callback_exchanges_and_refreshes_on_injected_token_host() {
    let mock = MockHttp::new();
    mock.push(
        "/oauth/token",
        200,
        token_json(&chatgpt_jwt("acct_live"), "refresh-1", 3600),
    );
    mock.push(
        "/oauth/token",
        200,
        token_json(&chatgpt_jwt("acct_live"), "refresh-2", 3600),
    );
    let port = mock.bind();
    let directory = std::env::temp_dir().join(format!("waku-codex-http-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(MemoryCredentialStore::default());
    let service = AuthService::new(AuthRuntime::testing(
        &directory,
        Arc::clone(&store) as Arc<dyn waku_core::auth::CredentialStore>,
        injected_codex_endpoints(port),
    ))
    .unwrap();

    let phase = service
        .start_login(ProviderId::new("openai-codex"), LoginMethod::OauthBrowser)
        .unwrap();
    let AuthPhase::AwaitingBrowser { login_id, .. } = phase else {
        panic!("{phase:?}");
    };
    let first = service.poll_login(login_id).unwrap();
    assert!(matches!(first, AuthPhase::AwaitingBrowser { .. }));

    let addr = service.browser_callback_addr(login_id).expect("listener");
    let state = service.browser_oauth_state(login_id).unwrap();
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "GET /auth/callback?code=browser-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    let completed = wait_login_settled(&service, login_id);
    assert!(
        matches!(completed, AuthPhase::Completed { .. }),
        "{completed:?}"
    );
    let exchange = &mock.recorded()[0];
    assert!(exchange.path.starts_with("/oauth/token"));
    assert!(exchange.body.contains("grant_type=authorization_code"));
    assert!(exchange.body.contains("code=browser-code"));
    assert!(exchange.body.contains("code_verifier="));
    assert!(
        exchange
            .body
            .contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback")
    );
    assert!(
        exchange
            .body
            .contains(&format!("client_id={}", AuthEndpoints::CODEX_CLIENT_ID))
    );
    let stored = store
        .get(&ProviderId::new("openai-codex"))
        .unwrap()
        .expect("stored");
    let StoredCredential::Oauth {
        refresh,
        account_id,
        ..
    } = stored
    else {
        panic!("expected oauth");
    };
    assert_eq!(refresh, "refresh-1");
    assert_eq!(account_id.as_deref(), Some("acct_live"));

    store
        .set(
            &ProviderId::new("openai-codex"),
            StoredCredential::Oauth {
                access: chatgpt_jwt("acct_live"),
                refresh: "refresh-1".into(),
                expires_at_ms: 1,
                account_id: Some("acct_live".into()),
                email: None,
            },
        )
        .unwrap();
    service.set_clock_ms(80_000);
    let endpoint = ProviderPreset::OpenAiCodex.endpoint();
    let (auth, extra) = service
        .resolve(&ProviderId::new("openai-codex"), &endpoint)
        .unwrap();
    assert!(matches!(auth, waku_harness::Auth::Bearer(_)));
    assert!(
        extra
            .iter()
            .any(|(name, value)| name == "chatgpt-account-id" && value == "acct_live")
    );
    let refresh_req = mock
        .recorded()
        .into_iter()
        .find(|row| row.body.contains("grant_type=refresh_token"))
        .expect("refresh");
    assert!(refresh_req.body.contains("refresh_token=refresh-1"));
}

#[test]
fn codex_device_pending_then_exchanges_authorization_code() {
    let mock = MockHttp::new();
    mock.push(
        "/device/usercode",
        200,
        json!({
            "device_auth_id": "dev-1",
            "user_code": "WAKU-1",
            "interval": 1
        })
        .to_string(),
    );
    mock.push("/device/token", 403, "{}");
    mock.push(
        "/device/token",
        200,
        json!({
            "authorization_code": "dev-code",
            "code_verifier": "dev-verifier"
        })
        .to_string(),
    );
    mock.push(
        "/oauth/token",
        200,
        token_json(&chatgpt_jwt("acct_dev"), "refresh-dev", 3600),
    );
    let port = mock.bind();
    let directory = std::env::temp_dir().join(format!("waku-codex-dev-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let service = AuthService::new(AuthRuntime::testing(
        &directory,
        Arc::new(MemoryCredentialStore::default()),
        injected_codex_endpoints(port),
    ))
    .unwrap();
    service.set_clock_ms(1_000);
    let phase = service
        .start_login(ProviderId::new("openai-codex"), LoginMethod::OauthDevice)
        .unwrap();
    let AuthPhase::AwaitingDevice { login_id, .. } = phase else {
        panic!("{phase:?}");
    };
    assert!(
        mock.recorded()
            .iter()
            .any(|row| row.path.starts_with("/device/usercode"))
    );
    let waiting = service.poll_login(login_id).unwrap();
    assert!(matches!(waiting, AuthPhase::AwaitingDevice { .. }));
    assert!(
        !mock
            .recorded()
            .iter()
            .any(|row| row.path.starts_with("/device/token"))
    );

    service.set_clock_ms(1_000 + 4_000);
    let pending = service.poll_login(login_id).unwrap();
    assert!(matches!(pending, AuthPhase::AwaitingDevice { .. }));
    service.set_clock_ms(1_000 + 4_000 + 1_000);
    let done = service.poll_login(login_id).unwrap();
    assert!(matches!(done, AuthPhase::Completed { .. }), "{done:?}");
    let exchange = mock
        .recorded()
        .into_iter()
        .find(|row| row.path.starts_with("/oauth/token"))
        .expect("device token exchange");
    assert!(exchange.body.contains("grant_type=authorization_code"));
    assert!(exchange.body.contains("code=dev-code"));
    assert!(exchange.body.contains("code_verifier=dev-verifier"));
}

#[test]
fn xai_device_pending_slow_down_refresh_and_invalid_grant() {
    let mock = MockHttp::new();
    let port = mock.bind();
    mock.push(
        "/.well-known/openid-configuration",
        200,
        json!({ "token_endpoint": format!("http://127.0.0.1:{port}/oauth2/token") }).to_string(),
    );
    mock.push(
        "/oauth2/device/code",
        200,
        json!({
            "device_code": "dc-1",
            "user_code": "XAI-1",
            "verification_uri_complete": "http://127.0.0.1/verify",
            "expires_in": 600,
            "interval": 2
        })
        .to_string(),
    );
    mock.push(
        "/oauth2/token",
        400,
        json!({ "error": "authorization_pending" }).to_string(),
    );
    mock.push(
        "/oauth2/token",
        400,
        json!({ "error": "slow_down" }).to_string(),
    );
    mock.push(
        "/oauth2/token",
        200,
        json!({
            "access_token": "xai-access-1",
            "refresh_token": "xai-refresh-1",
            "expires_in": 3600
        })
        .to_string(),
    );
    mock.push(
        "/.well-known/openid-configuration",
        200,
        json!({ "token_endpoint": format!("http://127.0.0.1:{port}/oauth2/token") }).to_string(),
    );
    mock.push(
        "/oauth2/token",
        200,
        json!({
            "access_token": "xai-access-2",
            "expires_in": 3600
        })
        .to_string(),
    );
    mock.push(
        "/.well-known/openid-configuration",
        200,
        json!({ "token_endpoint": format!("http://127.0.0.1:{port}/oauth2/token") }).to_string(),
    );
    mock.push(
        "/oauth2/token",
        400,
        json!({ "error": "invalid_grant" }).to_string(),
    );

    let directory = std::env::temp_dir().join(format!("waku-xai-http-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let store = Arc::new(MemoryCredentialStore::default());
    let mut endpoints = injected_codex_endpoints(port);
    endpoints.xai_discovery = format!("http://127.0.0.1:{port}/.well-known/openid-configuration");
    endpoints.xai_device_code = format!("http://127.0.0.1:{port}/oauth2/device/code");
    let service = AuthService::new(AuthRuntime::testing(
        &directory,
        Arc::clone(&store) as Arc<dyn waku_core::auth::CredentialStore>,
        endpoints,
    ))
    .unwrap();
    service.set_clock_ms(10_000);
    let phase = service
        .start_login(ProviderId::new("xai-oauth"), LoginMethod::OauthDevice)
        .unwrap();
    let AuthPhase::AwaitingDevice { login_id, .. } = phase else {
        panic!("{phase:?}");
    };
    assert!(matches!(
        service.poll_login(login_id).unwrap(),
        AuthPhase::AwaitingDevice { .. }
    ));
    service.set_clock_ms(12_000);
    assert!(matches!(
        service.poll_login(login_id).unwrap(),
        AuthPhase::AwaitingDevice { .. }
    ));
    service.set_clock_ms(14_000);
    assert!(matches!(
        service.poll_login(login_id).unwrap(),
        AuthPhase::AwaitingDevice { .. }
    ));
    service.set_clock_ms(21_000);
    let done = service.poll_login(login_id).unwrap();
    assert!(matches!(done, AuthPhase::Completed { .. }), "{done:?}");
    let StoredCredential::Oauth {
        access, refresh, ..
    } = store.get(&ProviderId::new("xai-oauth")).unwrap().unwrap()
    else {
        panic!("oauth");
    };
    assert_eq!(access, "xai-access-1");
    assert_eq!(refresh, "xai-refresh-1");

    store
        .set(
            &ProviderId::new("xai-oauth"),
            StoredCredential::Oauth {
                access: "xai-access-1".into(),
                refresh: "xai-refresh-1".into(),
                expires_at_ms: 1,
                account_id: None,
                email: None,
            },
        )
        .unwrap();
    service.set_clock_ms(90_000);
    let (auth, extra) = service
        .resolve(
            &ProviderId::new("xai-oauth"),
            &ProviderPreset::XaiOauth.endpoint(),
        )
        .unwrap();
    assert!(matches!(auth, waku_harness::Auth::Bearer(token) if token == "xai-access-2"));
    assert!(extra.is_empty());
    let StoredCredential::Oauth { refresh, .. } =
        store.get(&ProviderId::new("xai-oauth")).unwrap().unwrap()
    else {
        panic!("oauth");
    };
    assert_eq!(refresh, "xai-refresh-1");

    store
        .set(
            &ProviderId::new("xai-oauth"),
            StoredCredential::Oauth {
                access: "xai-access-2".into(),
                refresh: "xai-refresh-1".into(),
                expires_at_ms: 1,
                account_id: None,
                email: None,
            },
        )
        .unwrap();
    let err = service
        .resolve(
            &ProviderId::new("xai-oauth"),
            &ProviderPreset::XaiOauth.endpoint(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("sign-in"), "{err}");
}

#[test]
fn client_status_returns_browser_phase_and_busy_device_fallback() {
    let mock = MockHttp::new();
    mock.push(
        "/device/usercode",
        200,
        json!({
            "device_auth_id": "dev-busy",
            "user_code": "BUSY-1",
            "interval": 5
        })
        .to_string(),
    );
    let port = mock.bind();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = occupied.local_addr().unwrap();
    let directory = std::env::temp_dir().join(format!("waku-phase-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
    let store = StateStore::daemon(directory.join("app.db"));
    let mut runtime = AuthRuntime::testing(
        &directory,
        Arc::new(MemoryCredentialStore::default()),
        injected_codex_endpoints(port),
    );
    runtime.callback_bind = busy;
    let auth = AuthService::new(runtime).unwrap();
    let backend = WakuBackend::new_with_auth(settings, store, auth).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
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
    let client = DaemonClient::connect(&address, TOKEN.into()).unwrap();
    let ResponsePayload::Login { phase } = client
        .start_login(ProviderId::new("openai-codex"), LoginMethod::OauthBrowser)
        .unwrap()
    else {
        panic!("login");
    };
    match phase {
        AuthPhase::Failed {
            provider, message, ..
        } => {
            assert!(message.contains("device login"), "{message}");
            assert_eq!(provider.as_str(), "openai-codex");
        }
        other => panic!("{other:?}"),
    }
    let ResponsePayload::AuthStatus { phases, statuses } = client
        .get_auth_status(Some(ProviderId::new("openai-codex")))
        .unwrap()
    else {
        panic!("status");
    };
    let codex = statuses
        .iter()
        .find(|status| status.provider.as_str() == "openai-codex")
        .expect("codex status");
    assert_eq!(codex.method, AuthMethod::None);
    assert!(
        phases.iter().any(|phase| matches!(
            phase,
            AuthPhase::Failed {
                provider,
                message,
                ..
            } if provider.as_str() == "openai-codex" && message.contains("device login")
        )),
        "{phases:?}"
    );
    let ResponsePayload::Login { phase } = client
        .start_login(ProviderId::new("openai-codex"), LoginMethod::OauthDevice)
        .unwrap()
    else {
        panic!("device");
    };
    match phase {
        AuthPhase::AwaitingDevice { user_code, .. } => assert_eq!(user_code, "BUSY-1"),
        other => panic!("{other:?}"),
    }
    shutdown.store(true, Ordering::Release);
}

#[test]
fn opencode_go_apply_switches_responses_to_anthropic_without_service_tier() {
    let session = LiveSession::start(
        "opencode-go-route",
        ProviderPreset::OpenCodeGo.id(),
        "gpt-5",
        StoredCredential::api_key(SecretString::new("go-secret")),
        Some(waku_protocol::ServiceTier::Priority),
        &[
            ("/v1/responses", sse_responses("one")),
            ("/v1/messages", sse_anthropic("two")),
        ],
        |base| {
            let responses = opencode_entry(ProviderPreset::OpenCodeGo, "gpt-5", "GPT-5", base);
            let anthropic = opencode_entry(
                ProviderPreset::OpenCodeGo,
                "claude-sonnet-4-5",
                "Claude",
                base,
            );
            let gemini =
                opencode_entry(ProviderPreset::OpenCodeGo, "gemini-3-flash", "Gemini", base);
            assert_eq!(
                responses.api_format,
                waku_protocol::ApiFormat::OpenAiResponses
            );
            assert_eq!(anthropic.api_format, waku_protocol::ApiFormat::Anthropic);
            assert!(!responses.capabilities.service_tier);
            assert!(!anthropic.capabilities.service_tier);
            assert!(!gemini.supported);
            vec![responses, anthropic, gemini]
        },
    );
    session.prompt("first");
    let first = session
        .recorded()
        .into_iter()
        .find(|row| row.path.contains("/responses"))
        .expect("responses request");
    assert_routed_request(&first, "/responses", "gpt-5", None);
    assert_eq!(first.authorization.as_deref(), Some("Bearer go-secret"));

    assert!(
        session
            .handle
            .apply_options(waku_client::driver::SessionOptions {
                mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Build,
                model: Some("claude-sonnet-4-5".into()),
                reasoning_effort: None,
                service_tier: Some(waku_protocol::ServiceTier::Priority),
                context_window: None,
            })
    );
    session.prompt("second");
    let second = session
        .recorded()
        .into_iter()
        .find(|row| row.path.contains("/messages"))
        .expect("anthropic request");
    assert_routed_request(&second, "/messages", "claude-sonnet-4-5", None);
    assert_eq!(second.api_key.as_deref(), Some("go-secret"));

    let before = session.recorded().len();
    let applied = session
        .handle
        .apply_options(waku_client::driver::SessionOptions {
            mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Build,
            model: Some("gemini-3-flash".into()),
            reasoning_effort: None,
            service_tier: Some(waku_protocol::ServiceTier::Priority),
            context_window: None,
        });
    assert!(!applied);
    assert_eq!(session.recorded().len(), before);
}

#[test]
fn opencode_zen_responses_omits_service_tier() {
    let session = LiveSession::start(
        "opencode-zen-route",
        ProviderPreset::OpenCodeZen.id(),
        "gpt-5",
        StoredCredential::api_key(SecretString::new("zen-secret")),
        Some(waku_protocol::ServiceTier::Priority),
        &[("/v1/responses", sse_responses("zen"))],
        |base| {
            vec![opencode_entry(
                ProviderPreset::OpenCodeZen,
                "gpt-5",
                "GPT-5",
                base,
            )]
        },
    );
    session.prompt("zen");
    let request = session
        .recorded()
        .into_iter()
        .find(|row| row.path.contains("/responses"))
        .expect("zen responses request");
    assert_routed_request(&request, "/responses", "gpt-5", None);
    assert_eq!(request.authorization.as_deref(), Some("Bearer zen-secret"));
}

#[test]
fn xai_api_key_responses_omits_service_tier() {
    let session = LiveSession::start(
        "xai-key-route",
        ProviderPreset::Xai.id(),
        "grok-4.5",
        StoredCredential::api_key(SecretString::new("xai-secret")),
        Some(waku_protocol::ServiceTier::Priority),
        &[("/v1/responses", sse_responses("xai"))],
        |base| {
            let entry = apply_xai_policy(
                catalog_stub(ProviderPreset::Xai.id(), "grok-4.5", "Grok 4.5", base),
                false,
            );
            assert_eq!(entry.api_format, waku_protocol::ApiFormat::OpenAiResponses);
            assert!(!entry.capabilities.service_tier);
            vec![entry]
        },
    );
    session.prompt("xai-key");
    let request = session
        .recorded()
        .into_iter()
        .find(|row| row.path.contains("/responses"))
        .expect("xai request");
    assert_routed_request(&request, "/responses", "grok-4.5", None);
    assert_eq!(request.authorization.as_deref(), Some("Bearer xai-secret"));
}

#[test]
fn xai_oauth_responses_omits_service_tier() {
    let session = LiveSession::start(
        "xai-oauth-route",
        ProviderPreset::XaiOauth.id(),
        "grok-4.5",
        StoredCredential::Oauth {
            access: "xai-oauth-access".into(),
            refresh: "xai-oauth-refresh".into(),
            expires_at_ms: u64::MAX,
            account_id: None,
            email: None,
        },
        Some(waku_protocol::ServiceTier::Priority),
        &[("/v1/responses", sse_responses("oauth"))],
        |base| {
            let models = xai_oauth_seed(base);
            assert!(
                models
                    .iter()
                    .any(|entry| entry.id == "grok-4.5" && !entry.capabilities.service_tier)
            );
            models
        },
    );
    session.prompt("xai-oauth");
    let request = session
        .recorded()
        .into_iter()
        .find(|row| row.path.contains("/responses"))
        .expect("xai oauth request");
    assert_routed_request(&request, "/responses", "grok-4.5", None);
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer xai-oauth-access")
    );
}

#[test]
fn openai_official_responses_sends_requested_service_tier() {
    let mut entry_base = None;
    let session = LiveSession::start(
        "openai-official-tier",
        ProviderPreset::OpenAiResponses.id(),
        "gpt-5",
        StoredCredential::api_key(SecretString::new("sk-official")),
        Some(waku_protocol::ServiceTier::Priority),
        &[("/v1/responses", sse_responses("official"))],
        |base| {
            let mut entry =
                catalog_stub(ProviderPreset::OpenAiResponses.id(), "gpt-5", "GPT-5", base);
            entry.capabilities =
                ModelCapabilities::openai_api(waku_protocol::ApiFormat::OpenAiResponses);
            entry_base = Some(entry.capabilities.service_tier);
            vec![entry]
        },
    );
    assert_eq!(entry_base, Some(true));
    session.prompt("official");
    let request = session
        .recorded()
        .into_iter()
        .find(|row| row.path.contains("/responses"))
        .expect("official responses request");
    assert_routed_request(&request, "/responses", "gpt-5", Some("priority"));
    assert_eq!(request.authorization.as_deref(), Some("Bearer sk-official"));
}

#[test]
fn openai_official_chat_sends_requested_service_tier() {
    let session = LiveSession::start(
        "openai-official-chat-tier",
        ProviderPreset::OpenAiChat.id(),
        "gpt-5",
        StoredCredential::api_key(SecretString::new("sk-chat")),
        Some(waku_protocol::ServiceTier::Flex),
        &[("/v1/chat/completions", sse_chat("official-chat"))],
        |base| {
            let mut entry = catalog_stub(ProviderPreset::OpenAiChat.id(), "gpt-5", "GPT-5", base);
            entry.api_format = waku_protocol::ApiFormat::OpenAiChat;
            entry.capabilities =
                ModelCapabilities::openai_api(waku_protocol::ApiFormat::OpenAiChat);
            assert!(entry.capabilities.service_tier);
            vec![entry]
        },
    );
    session.prompt("official-chat");
    let request = session
        .recorded()
        .into_iter()
        .find(|row| row.path.contains("/chat/completions"))
        .expect("official chat request");
    assert_routed_request(&request, "/chat/completions", "gpt-5", Some("flex"));
    assert_eq!(request.authorization.as_deref(), Some("Bearer sk-chat"));
}

#[test]
fn openai_codex_responses_omits_service_tier() {
    let session = LiveSession::start(
        "openai-codex-tier",
        ProviderPreset::OpenAiCodex.id(),
        "gpt-5",
        StoredCredential::Oauth {
            access: chatgpt_jwt("acct_live"),
            refresh: "refresh-1".into(),
            expires_at_ms: u64::MAX,
            account_id: Some("acct_live".into()),
            email: None,
        },
        Some(waku_protocol::ServiceTier::Priority),
        &[("/v1/responses", sse_responses("codex"))],
        |base| {
            let mut entry = catalog_stub(ProviderPreset::OpenAiCodex.id(), "gpt-5", "GPT-5", base);
            entry.transport = TransportProfile::Codex;
            entry.capabilities = ModelCapabilities::codex();
            assert_eq!(entry.api_format, waku_protocol::ApiFormat::OpenAiResponses);
            assert!(!entry.capabilities.service_tier);
            vec![entry]
        },
    );
    session.prompt("codex");
    let request = session
        .recorded()
        .into_iter()
        .find(|row| row.path.contains("/responses"))
        .expect("codex responses request");
    assert_routed_request(&request, "/responses", "gpt-5", None);
}

#[test]
fn expired_oauth_refreshes_before_second_prompt() {
    let mock = MockHttp::new();
    mock.push("/v1/responses", 200, sse_responses("alpha"));
    mock.push("/v1/responses", 200, sse_responses("beta"));
    mock.push(
        "/oauth/token",
        200,
        token_json(&chatgpt_jwt("acct_live"), "refresh-next", 3600),
    );
    let port = mock.bind();
    let directory = std::env::temp_dir().join(format!("waku-refresh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let workspace = directory.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let creds = Arc::new(MemoryCredentialStore::default());
    creds
        .set(
            &ProviderId::new("openai-codex"),
            StoredCredential::Oauth {
                access: chatgpt_jwt("acct_live"),
                refresh: "refresh-first".into(),
                expires_at_ms: u64::MAX,
                account_id: Some("acct_live".into()),
                email: None,
            },
        )
        .unwrap();
    let mut endpoints = injected_codex_endpoints(port);
    endpoints.openai_token = format!("http://127.0.0.1:{port}/oauth/token");
    let persist_dir = directory.clone();
    waku_core::auth::AuthRuntime::testing(
        &persist_dir,
        Arc::clone(&creds) as Arc<dyn waku_core::auth::CredentialStore>,
        endpoints.clone(),
    )
    .persist
    .put_catalog(
        &ProviderId::new("openai-codex"),
        vec![ModelCatalogEntry {
            provider: ProviderId::new("openai-codex"),
            id: "gpt-5".into(),
            name: "GPT".into(),
            api_format: waku_protocol::ApiFormat::OpenAiResponses,
            transport: TransportProfile::Codex,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            context_window: 128_000,
            max_output_tokens: 8_192,
            reasoning: true,
            capabilities: ModelCapabilities::codex(),
            supported: true,
            unsupported_reason: None,
        }],
        CatalogSource::Live,
        1,
    )
    .unwrap();

    let state_store = StateStore::daemon(directory.join("app.db"));
    let mut state = PersistedState::fresh(workspace.clone());
    state.sessions[0].provider = ProviderId::new("openai-codex");
    state.sessions[0].model = Some("gpt-5".into());
    state.sessions[0].begin_turn("refresh");
    let session_id = state.sessions[0].id;
    state.mark_session_dirty(session_id);
    state_store.save(&mut state).unwrap();
    let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
    let runtime = AuthRuntime::testing(
        &directory,
        Arc::clone(&creds) as Arc<dyn waku_core::auth::CredentialStore>,
        endpoints,
    );
    let backend =
        WakuBackend::new_with_auth(settings, state_store, AuthService::new(runtime).unwrap())
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
    let (wake, _) = smol::channel::bounded(1);
    let (events, rx) = event_channel(wake);
    let handle = DriverHandle::start(
        client,
        session_id,
        DriverStartOptions {
            provider: ProviderId::new("openai-codex"),
            cwd: workspace,
            mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Build,
            model: Some("gpt-5".into()),
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
        },
        events,
    )
    .unwrap();
    wait_connected(&rx);
    handle.prompt(PromptInput::text("first"));
    wait_finished(&rx);
    assert!(
        !mock
            .recorded()
            .iter()
            .any(|row| row.body.contains("grant_type=refresh_token"))
    );

    creds
        .set(
            &ProviderId::new("openai-codex"),
            StoredCredential::Oauth {
                access: chatgpt_jwt("acct_live"),
                refresh: "refresh-first".into(),
                expires_at_ms: 1,
                account_id: Some("acct_live".into()),
                email: None,
            },
        )
        .unwrap();
    handle.prompt(PromptInput::text("second"));
    wait_finished(&rx);
    assert!(
        mock.recorded()
            .iter()
            .any(|row| row.body.contains("grant_type=refresh_token")),
        "second prompt must refresh"
    );
    shutdown.store(true, Ordering::Release);
}

fn wait_connected(rx: &crossbeam_channel::Receiver<waku_protocol::model::DriverEvent>) {
    wait_event(rx, |event| {
        matches!(event, waku_protocol::model::DriverEvent::Connected)
    });
}

fn wait_finished(rx: &crossbeam_channel::Receiver<waku_protocol::model::DriverEvent>) {
    wait_event(rx, |event| {
        matches!(
            event,
            waku_protocol::model::DriverEvent::TurnFinished { .. }
        )
    });
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
