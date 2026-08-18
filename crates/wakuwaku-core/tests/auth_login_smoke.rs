//! Daemon/client auth status, API-key login, catalog refresh, and logout.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use uuid::Uuid;
use wakuwaku_client::DaemonClient;
use wakuwaku_core::auth::{AuthRuntime, AuthService, MemoryCredentialStore};
use wakuwaku_core::daemon::WakuBackend;
use wakuwaku_core::persistence::StateStore;
use wakuwaku_core::{DaemonSettingsStore, ServerOptions, serve};
use wakuwaku_protocol::{
    AuthEndpoints, AuthMethod, AuthPhase, CatalogSource, LoginMethod, ProviderId, ResponsePayload,
    SecretString,
};

const TOKEN: &str = "auth-smoke-token";

fn bind_json_server(body: serde_json::Value) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let bytes = serde_json::to_vec(&body).unwrap();
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&bytes);
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

fn start_daemon(
    directory: &std::path::Path,
    endpoints: AuthEndpoints,
    model_base: Option<(&str, String)>,
) -> (String, Arc<AtomicBool>) {
    let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
    let store = StateStore::daemon(directory.join("app.db"));
    let mut runtime = AuthRuntime::testing(
        directory,
        Arc::new(MemoryCredentialStore::default()),
        endpoints,
    );
    if let Some((id, base)) = model_base {
        runtime.model_base_overrides.insert(id.to_owned(), base);
    }
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
    (address, shutdown)
}

#[test]
fn api_key_login_refreshes_models_and_logout_clears_status() {
    let port = bind_json_server(json!({ "data": [{ "id": "grok-4.5", "name": "Grok 4.5" }] }));
    let directory = std::env::temp_dir().join(format!("wakuwaku-auth-smoke-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let (address, shutdown) = start_daemon(
        &directory,
        AuthEndpoints::production(),
        Some(("xai", format!("http://127.0.0.1:{port}/v1"))),
    );
    let client = DaemonClient::connect(&address, TOKEN.into()).unwrap();

    let ResponsePayload::AuthStatus { statuses, .. } = client
        .get_auth_status(Some(ProviderId::new("xai")))
        .unwrap()
    else {
        panic!("expected auth status");
    };
    assert_eq!(statuses[0].method, AuthMethod::None);

    let ResponsePayload::Login { phase } = client
        .start_login(
            ProviderId::new("xai"),
            wakuwaku_protocol::LoginMethod::ApiKey,
        )
        .unwrap()
    else {
        panic!("expected start");
    };
    let AuthPhase::AwaitingApiKey {
        login_id, provider, ..
    } = phase
    else {
        panic!("expected awaiting api key: {phase:?}");
    };
    assert_eq!(provider.as_str(), "xai");
    let ResponsePayload::Login { phase } = client
        .complete_api_key_login(
            login_id,
            ProviderId::new("xai"),
            SecretString::new("xai-test-key"),
        )
        .unwrap()
    else {
        panic!("expected login");
    };
    let AuthPhase::Completed {
        login_id: done_id,
        provider,
    } = phase
    else {
        panic!("expected completed: {phase:?}");
    };
    assert_eq!(done_id, login_id);
    assert_eq!(provider.as_str(), "xai");

    let ResponsePayload::Models { catalog } =
        client.refresh_models(ProviderId::new("xai")).unwrap()
    else {
        panic!("expected models");
    };
    assert_eq!(catalog.source, CatalogSource::Live);
    assert_eq!(catalog.models[0].id, "grok-4.5");

    let ResponsePayload::AuthStatus { statuses, .. } = client
        .get_auth_status(Some(ProviderId::new("xai")))
        .unwrap()
    else {
        panic!("expected auth status");
    };
    assert_eq!(statuses[0].method, AuthMethod::StoredApiKey);

    client.logout(ProviderId::new("xai")).unwrap();
    let ResponsePayload::AuthStatus { statuses, .. } = client
        .get_auth_status(Some(ProviderId::new("xai")))
        .unwrap()
    else {
        panic!("expected auth status");
    };
    assert_eq!(statuses[0].method, AuthMethod::None);
    shutdown.store(true, Ordering::Release);
}

#[test]
fn oauth_device_start_is_invoked_for_supergrok() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-oauth-smoke-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let (address, shutdown) = start_daemon(&directory, AuthEndpoints::production(), None);
    let client = DaemonClient::connect(&address, TOKEN.into()).unwrap();
    let result = client.start_login(ProviderId::new("xai-oauth"), LoginMethod::OauthDevice);
    match result {
        Ok(ResponsePayload::Login { phase }) => {
            assert!(matches!(
                phase,
                AuthPhase::AwaitingDevice { .. } | AuthPhase::Failed { .. }
            ));
        }
        Err(error) => assert!(!error.to_string().is_empty()),
        Ok(other) => panic!("unexpected {other:?}"),
    }
    shutdown.store(true, Ordering::Release);
}

#[test]
fn get_auth_status_phases_name_their_provider() {
    let directory = std::env::temp_dir().join(format!("wakuwaku-auth-status-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let (address, shutdown) = start_daemon(&directory, AuthEndpoints::production(), None);
    let client = DaemonClient::connect(&address, TOKEN.into()).unwrap();

    let ResponsePayload::Login { phase: xai } = client
        .start_login(ProviderId::new("xai"), LoginMethod::ApiKey)
        .unwrap()
    else {
        panic!("expected xai start");
    };
    let ResponsePayload::Login { phase: anthropic } = client
        .start_login(ProviderId::new("anthropic"), LoginMethod::ApiKey)
        .unwrap()
    else {
        panic!("expected anthropic start");
    };
    let AuthPhase::AwaitingApiKey {
        login_id: xai_id,
        provider: xai_provider,
        ..
    } = xai
    else {
        panic!("{xai:?}");
    };
    let AuthPhase::AwaitingApiKey {
        login_id: anthropic_id,
        provider: anthropic_provider,
        ..
    } = anthropic
    else {
        panic!("{anthropic:?}");
    };
    assert_ne!(xai_id, anthropic_id);
    assert_eq!(xai_provider.as_str(), "xai");
    assert_eq!(anthropic_provider.as_str(), "anthropic");

    let ResponsePayload::AuthStatus { phases, .. } = client.get_auth_status(None).unwrap() else {
        panic!("expected unfiltered auth status");
    };
    assert_eq!(phases.len(), 2, "{phases:?}");
    assert!(
        phases.iter().any(|phase| {
            phase.provider().map(ProviderId::as_str) == Some("xai")
                && phase.login_id() == Some(xai_id)
        }),
        "{phases:?}"
    );
    assert!(
        phases.iter().any(|phase| {
            phase.provider().map(ProviderId::as_str) == Some("anthropic")
                && phase.login_id() == Some(anthropic_id)
        }),
        "{phases:?}"
    );

    let ResponsePayload::AuthStatus { phases, .. } = client
        .get_auth_status(Some(ProviderId::new("xai")))
        .unwrap()
    else {
        panic!("expected xai auth status");
    };
    assert_eq!(phases.len(), 1, "{phases:?}");
    assert_eq!(phases[0].provider().map(ProviderId::as_str), Some("xai"));
    assert_eq!(phases[0].login_id(), Some(xai_id));

    client.cancel_login(xai_id).unwrap();
    let ResponsePayload::AuthStatus { phases, .. } = client.get_auth_status(None).unwrap() else {
        panic!("expected remaining auth status");
    };
    assert_eq!(phases.len(), 1, "{phases:?}");
    assert_eq!(
        phases[0].provider().map(ProviderId::as_str),
        Some("anthropic")
    );
    assert_eq!(phases[0].login_id(), Some(anthropic_id));
    shutdown.store(true, Ordering::Release);
}
