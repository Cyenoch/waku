//! Auth service: login state, credential resolve, catalog refresh.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use uuid::Uuid;
use wakuwaku_harness::Auth;
use wakuwaku_protocol::xai_oauth_seed;
use wakuwaku_protocol::{
    AuthEndpoints, AuthMethod, AuthPhase, CatalogSource, ExternalProvider, LoginMethod,
    MODELS_DEV_API_URL, ModelCapabilities, ModelCatalog, ModelCatalogEntry, ProviderAuthStatus,
    ProviderId, ProviderLimits, ProviderPreset, SecretString, TransportProfile,
};

use super::error::AuthError;
use super::flows::{
    self, DevicePoll, http_client, models_dev_http_client, now_ms, stored_to_auth, validate_api_key,
};
use super::persist::{AuthPersist, PublicAuthRecord};
use super::pkce;
use super::store::{CredentialStore, StoredCredential, production_store};

pub struct AuthRuntime {
    pub store: Arc<dyn CredentialStore>,
    pub endpoints: AuthEndpoints,
    pub persist: AuthPersist,
    pub callback_bind: SocketAddr,
    pub model_base_overrides: HashMap<String, String>,
    pub models_dev_url: Option<String>,
}

impl AuthRuntime {
    pub fn production(directory: &Path) -> Result<Self, AuthError> {
        Ok(Self {
            store: production_store(directory)?,
            endpoints: AuthEndpoints::production(),
            persist: AuthPersist::new(directory),
            callback_bind: SocketAddr::from(([127, 0, 0, 1], AuthEndpoints::CODEX_CALLBACK_PORT)),
            model_base_overrides: HashMap::new(),
            models_dev_url: Some(MODELS_DEV_API_URL.to_owned()),
        })
    }

    pub fn testing(
        directory: &Path,
        store: Arc<dyn CredentialStore>,
        endpoints: AuthEndpoints,
    ) -> Self {
        Self {
            store,
            endpoints,
            persist: AuthPersist::new(directory),
            callback_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            model_base_overrides: HashMap::new(),
            models_dev_url: None,
        }
    }
}

type ResolvedProviderOverlay = (
    ExternalProvider,
    TransportProfile,
    Auth,
    Vec<(String, String)>,
    ModelCapabilities,
    ProviderLimits,
);

struct BrowserCallback {
    shutdown: Arc<AtomicBool>,
    received: Arc<parking_lot::Mutex<Option<(String, String)>>>,
    local_addr: SocketAddr,
    join: parking_lot::Mutex<Option<thread::JoinHandle<()>>>,
}

struct LoginSession {
    provider: ProviderId,
    method: LoginMethod,
    phase: AuthPhase,
    verifier: Option<String>,
    state: Option<String>,
    device_auth_id: Option<String>,
    user_code: Option<String>,
    xai_token_endpoint: Option<String>,
    xai_device_code: Option<String>,
    interval_ms: u64,
    expires_at_ms: Option<u64>,
    next_poll_at_ms: u64,
}

impl LoginSession {
    fn new(provider: ProviderId, method: LoginMethod, phase: AuthPhase) -> Self {
        Self {
            provider,
            method,
            phase,
            verifier: None,
            state: None,
            device_auth_id: None,
            user_code: None,
            xai_token_endpoint: None,
            xai_device_code: None,
            interval_ms: 5_000,
            expires_at_ms: None,
            next_poll_at_ms: 0,
        }
    }
}

pub struct AuthService {
    runtime: AuthRuntime,
    http: reqwest::blocking::Client,
    models_dev_http: reqwest::blocking::Client,
    logins: Mutex<HashMap<Uuid, LoginSession>>,
    refresh_lock: Mutex<()>,
    customs: Mutex<Vec<ExternalProvider>>,
    clock_ms: AtomicU64,
    callbacks: Mutex<HashMap<Uuid, Arc<BrowserCallback>>>,
}

impl AuthService {
    pub fn new(runtime: AuthRuntime) -> Result<Self, AuthError> {
        Ok(Self {
            http: http_client()?,
            models_dev_http: models_dev_http_client()?,
            runtime,
            logins: Mutex::new(HashMap::new()),
            refresh_lock: Mutex::new(()),
            customs: Mutex::new(Vec::new()),
            clock_ms: AtomicU64::new(0),
            callbacks: Mutex::new(HashMap::new()),
        })
    }

    pub fn set_custom_providers(&self, providers: Vec<ExternalProvider>) {
        *self.customs.lock() = providers;
    }

    pub fn set_clock_ms(&self, now: u64) {
        self.clock_ms.store(now, Ordering::SeqCst);
    }

    pub fn browser_callback_addr(&self, login_id: Uuid) -> Option<SocketAddr> {
        self.callbacks
            .lock()
            .get(&login_id)
            .map(|callback| callback.local_addr)
    }

    pub fn browser_oauth_state(&self, login_id: Uuid) -> Option<String> {
        self.logins
            .lock()
            .get(&login_id)
            .and_then(|login| login.state.clone())
    }

    fn now(&self) -> u64 {
        match self.clock_ms.load(Ordering::SeqCst) {
            0 => now_ms(),
            value => value,
        }
    }

    fn login_method_allowed(provider: &ProviderId, method: LoginMethod) -> bool {
        match ProviderPreset::parse_id(provider.as_str()) {
            Some(ProviderPreset::OpenAiCodex) => {
                matches!(method, LoginMethod::OauthBrowser | LoginMethod::OauthDevice)
            }
            Some(ProviderPreset::XaiOauth) => matches!(method, LoginMethod::OauthDevice),
            Some(_) | None => matches!(method, LoginMethod::ApiKey),
        }
    }

    fn first_poll_delay_ms(provider: &ProviderId, interval_ms: u64) -> u64 {
        if ProviderPreset::parse_id(provider.as_str()) == Some(ProviderPreset::OpenAiCodex) {
            interval_ms.saturating_add(3_000).min(5_000)
        } else {
            interval_ms
        }
    }

    pub fn poll_active_logins(&self) -> Vec<AuthPhase> {
        let mut phases = Vec::new();
        for login_id in self.active_login_ids() {
            match self.poll_login(login_id) {
                Ok(phase) => phases.push(phase),
                Err(AuthError::NoActiveLogin) => {}
                Err(error) => {
                    let Some(provider) = self
                        .logins
                        .lock()
                        .get(&login_id)
                        .map(|login| login.provider.clone())
                    else {
                        continue;
                    };
                    let phase = AuthPhase::Failed {
                        login_id,
                        provider,
                        message: error.to_string(),
                    };
                    if let Some(login) = self.logins.lock().get_mut(&login_id) {
                        login.phase = phase.clone();
                    }
                    phases.push(phase);
                }
            }
        }
        phases
    }

    pub fn auth_phases(&self, provider: Option<&ProviderId>) -> Vec<AuthPhase> {
        let phases = self.poll_active_logins();
        match provider {
            Some(provider) => phases
                .into_iter()
                .filter(|phase| phase.provider() == Some(provider))
                .collect(),
            None => phases,
        }
    }

    pub fn status(&self, provider: Option<&ProviderId>) -> Vec<ProviderAuthStatus> {
        let accounts = self.runtime.persist.load_accounts();
        let ids: Vec<ProviderId> = if let Some(provider) = provider {
            vec![provider.clone()]
        } else {
            ProviderPreset::ALL
                .into_iter()
                .map(ProviderPreset::provider_id)
                .collect()
        };
        ids.into_iter()
            .map(|id| {
                let record = accounts
                    .accounts
                    .get(id.as_str())
                    .cloned()
                    .unwrap_or_default();
                let stored = self.runtime.store.get(&id).ok().flatten();
                let env_configured = ProviderPreset::parse_id(id.as_str())
                    .and_then(ProviderPreset::env_key)
                    .is_some_and(|name| {
                        std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
                    });
                let method = match (&stored, env_configured) {
                    (Some(StoredCredential::Oauth { .. }), _) => AuthMethod::Oauth,
                    (Some(StoredCredential::ApiKey { .. }), _) => AuthMethod::StoredApiKey,
                    (None, true) => AuthMethod::EnvKey,
                    (None, false) => AuthMethod::None,
                };
                ProviderAuthStatus {
                    provider: id,
                    method,
                    email: record.email,
                    account_id: record.account_id,
                    expires_at_ms: record.expires_at_ms,
                    relogin_required: record.relogin_required,
                }
            })
            .collect()
    }

    pub fn active_login_ids(&self) -> Vec<Uuid> {
        self.logins.lock().keys().copied().collect()
    }

    pub fn start_login(
        &self,
        provider: ProviderId,
        method: LoginMethod,
    ) -> Result<AuthPhase, AuthError> {
        if !Self::login_method_allowed(&provider, method) {
            return Err(AuthError::failed(format!(
                "login method {method:?} is not allowed for {}",
                provider.as_str()
            )));
        }
        let login_id = Uuid::new_v4();
        match method {
            LoginMethod::ApiKey => {
                let phase = AuthPhase::AwaitingApiKey {
                    login_id,
                    provider: provider.clone(),
                    instructions: format!("Paste the API key for {}", provider),
                };
                self.logins
                    .lock()
                    .insert(login_id, LoginSession::new(provider, method, phase.clone()));
                Ok(phase)
            }
            LoginMethod::OauthBrowser => self.start_codex_browser(login_id, provider),
            LoginMethod::OauthDevice => self.start_device(login_id, provider),
        }
    }

    fn start_codex_browser(
        &self,
        login_id: Uuid,
        provider: ProviderId,
    ) -> Result<AuthPhase, AuthError> {
        if ProviderPreset::parse_id(provider.as_str()) != Some(ProviderPreset::OpenAiCodex) {
            return Err(AuthError::failed(
                "browser OAuth is only implemented for ChatGPT Codex",
            ));
        }
        let pkce = pkce::generate_pkce()?;
        let state = Uuid::new_v4().to_string();
        let url = flows::codex_authorize_url(&self.runtime.endpoints, &state, &pkce.challenge);
        let phase = AuthPhase::AwaitingBrowser {
            login_id,
            provider: provider.clone(),
            url: url.clone(),
        };
        let callback = match self.spawn_browser_listener() {
            Ok(callback) => callback,
            Err(error) => {
                let phase = AuthPhase::Failed {
                    login_id,
                    provider: provider.clone(),
                    message: error.to_string(),
                };
                self.logins.lock().insert(
                    login_id,
                    LoginSession::new(provider, LoginMethod::OauthBrowser, phase.clone()),
                );
                return Ok(phase);
            }
        };
        self.callbacks
            .lock()
            .insert(login_id, Arc::clone(&callback));
        self.logins.lock().insert(
            login_id,
            LoginSession {
                verifier: Some(pkce.verifier),
                state: Some(state),
                ..LoginSession::new(provider, LoginMethod::OauthBrowser, phase.clone())
            },
        );
        Ok(phase)
    }

    fn start_device(&self, login_id: Uuid, provider: ProviderId) -> Result<AuthPhase, AuthError> {
        match ProviderPreset::parse_id(provider.as_str()) {
            Some(ProviderPreset::OpenAiCodex) => {
                let start = flows::start_codex_device(&self.http, &self.runtime.endpoints)?;
                let phase = AuthPhase::AwaitingDevice {
                    login_id,
                    provider: provider.clone(),
                    user_code: start.user_code.clone(),
                    verification_url: self.runtime.endpoints.openai_device_auth_url.clone(),
                    instructions: format!("Enter code: {}", start.user_code),
                };
                self.logins.lock().insert(
                    login_id,
                    LoginSession {
                        device_auth_id: Some(start.device_auth_id),
                        user_code: Some(start.user_code),
                        interval_ms: start.interval_ms,
                        next_poll_at_ms: self.now().saturating_add(Self::first_poll_delay_ms(
                            &provider,
                            start.interval_ms,
                        )),
                        ..LoginSession::new(provider, LoginMethod::OauthDevice, phase.clone())
                    },
                );
                Ok(phase)
            }
            Some(ProviderPreset::XaiOauth) => {
                let start = flows::start_xai_device(&self.http, &self.runtime.endpoints)?;
                let phase = AuthPhase::AwaitingDevice {
                    login_id,
                    provider: provider.clone(),
                    user_code: start.user_code.clone(),
                    verification_url: start.verification_url.clone(),
                    instructions: format!("Enter code: {}", start.user_code),
                };
                self.logins.lock().insert(
                    login_id,
                    LoginSession {
                        user_code: Some(start.user_code),
                        xai_token_endpoint: Some(start.token_endpoint),
                        xai_device_code: Some(start.device_code),
                        interval_ms: start.interval_ms.max(1_000),
                        expires_at_ms: Some(self.now().saturating_add(start.expires_in_ms)),
                        next_poll_at_ms: self.now().saturating_add(start.interval_ms.max(1_000)),
                        ..LoginSession::new(provider, LoginMethod::OauthDevice, phase.clone())
                    },
                );
                Ok(phase)
            }
            _ => Err(AuthError::failed(
                "device OAuth is only implemented for Codex and SuperGrok",
            )),
        }
    }

    pub fn poll_login(&self, login_id: Uuid) -> Result<AuthPhase, AuthError> {
        let snapshot = {
            let logins = self.logins.lock();
            logins.get(&login_id).map(|login| {
                (
                    login.provider.clone(),
                    login.method,
                    login.device_auth_id.clone(),
                    login.user_code.clone(),
                    login.xai_token_endpoint.clone(),
                    login.xai_device_code.clone(),
                    login.phase.clone(),
                    login.expires_at_ms,
                    login.next_poll_at_ms,
                )
            })
        };
        let Some((
            provider,
            method,
            device_auth_id,
            user_code,
            xai_token_endpoint,
            xai_device_code,
            current,
            expires_at_ms,
            next_poll_at_ms,
        )) = snapshot
        else {
            return Err(AuthError::NoActiveLogin);
        };
        if matches!(
            current,
            AuthPhase::Completed { .. } | AuthPhase::Failed { .. }
        ) {
            return Ok(current);
        }
        let now = self.now();
        if expires_at_ms.is_some_and(|expires| now >= expires) {
            let phase = AuthPhase::Failed {
                login_id,
                provider: provider.clone(),
                message: "device authorization expired".into(),
            };
            if let Some(login) = self.logins.lock().get_mut(&login_id) {
                login.phase = phase.clone();
            }
            return Ok(phase);
        }
        if now < next_poll_at_ms {
            return Ok(current);
        }
        let tokens = match method {
            LoginMethod::OauthDevice
                if ProviderPreset::parse_id(provider.as_str())
                    == Some(ProviderPreset::OpenAiCodex) =>
            {
                let (Some(device_auth_id), Some(user_code)) = (device_auth_id, user_code) else {
                    return Ok(current);
                };
                match flows::poll_codex_device(
                    &self.http,
                    &self.runtime.endpoints,
                    &device_auth_id,
                    &user_code,
                    self.now(),
                )? {
                    DevicePoll::Pending | DevicePoll::SlowDown => {
                        self.schedule_next_poll(login_id, false);
                        return Ok(current);
                    }
                    DevicePoll::Complete(tokens) => tokens,
                }
            }
            LoginMethod::OauthDevice
                if ProviderPreset::parse_id(provider.as_str())
                    == Some(ProviderPreset::XaiOauth) =>
            {
                let (Some(token_endpoint), Some(device_code)) =
                    (xai_token_endpoint, xai_device_code)
                else {
                    return Ok(current);
                };
                match flows::poll_xai_device(
                    &self.http,
                    &self.runtime.endpoints,
                    &token_endpoint,
                    &device_code,
                    self.now(),
                )? {
                    DevicePoll::Pending => {
                        self.schedule_next_poll(login_id, false);
                        return Ok(current);
                    }
                    DevicePoll::SlowDown => {
                        self.schedule_next_poll(login_id, true);
                        return Ok(current);
                    }
                    DevicePoll::Complete(tokens) => tokens,
                }
            }
            LoginMethod::OauthBrowser => {
                if let Some(tokens) = self.accept_codex_callback(login_id)? {
                    tokens
                } else {
                    return Ok(current);
                }
            }
            _ => return Ok(current),
        };
        self.store_oauth(&provider, tokens)?;
        let phase = AuthPhase::Completed {
            login_id,
            provider: provider.clone(),
        };
        if let Some(login) = self.logins.lock().get_mut(&login_id) {
            login.phase = phase.clone();
        }
        Ok(phase)
    }

    fn schedule_next_poll(&self, login_id: Uuid, slow_down: bool) {
        if let Some(login) = self.logins.lock().get_mut(&login_id) {
            if slow_down {
                login.interval_ms = login.interval_ms.saturating_add(5_000);
            }
            login.next_poll_at_ms = self.now().saturating_add(login.interval_ms);
        }
    }

    fn accept_codex_callback(
        &self,
        login_id: Uuid,
    ) -> Result<Option<flows::OauthTokens>, AuthError> {
        let Some((verifier, expected_state)) = self
            .logins
            .lock()
            .get(&login_id)
            .and_then(|login| Some((login.verifier.clone()?, login.state.clone()?)))
        else {
            return Ok(None);
        };
        let received = self
            .callbacks
            .lock()
            .get(&login_id)
            .and_then(|callback| callback.received.lock().take());
        let Some((code, state)) = received else {
            return Ok(None);
        };
        if state != expected_state {
            return Err(AuthError::failed("Codex callback state mismatch"));
        }
        flows::exchange_codex_code(
            &self.http,
            &self.runtime.endpoints,
            &code,
            &verifier,
            &AuthEndpoints::codex_callback_uri(),
            self.now(),
        )
        .map(Some)
    }

    fn spawn_browser_listener(&self) -> Result<Arc<BrowserCallback>, AuthError> {
        let listener = TcpListener::bind(self.runtime.callback_bind)
            .map_err(|_| AuthError::failed("Codex callback port 1455 is busy; use device login"))?;
        let local_addr = listener
            .local_addr()
            .map_err(|_| AuthError::failed("Codex callback port 1455 is busy; use device login"))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| AuthError::failed("could not listen for Codex callback"))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let received = Arc::new(parking_lot::Mutex::new(None));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_received = Arc::clone(&received);
        let join = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 4096];
                        let read = stream.read(&mut buf).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..read]);
                        let line = request.lines().next().unwrap_or("");
                        let path = line.split_whitespace().nth(1).unwrap_or("");
                        let url = format!("http://127.0.0.1{path}");
                        if let Ok(parsed) = reqwest::Url::parse(&url) {
                            let mut code = None;
                            let mut state = None;
                            for (key, value) in parsed.query_pairs() {
                                if key == "code" {
                                    code = Some(value.into_owned());
                                } else if key == "state" {
                                    state = Some(value.into_owned());
                                }
                            }
                            if let (Some(code), Some(state)) = (code, state) {
                                *thread_received.lock() = Some((code, state));
                            }
                        }
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Arc::new(BrowserCallback {
            shutdown,
            received,
            local_addr,
            join: parking_lot::Mutex::new(Some(join)),
        }))
    }

    fn shutdown_callback(&self, login_id: Uuid) {
        if let Some(callback) = self.callbacks.lock().remove(&login_id) {
            callback.shutdown.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect_timeout(&callback.local_addr, Duration::from_millis(50));
            if let Some(join) = callback.join.lock().take() {
                let _ = join.join();
            }
        }
    }

    pub fn complete_api_key(
        &self,
        login_id: Uuid,
        provider: ProviderId,
        key: SecretString,
    ) -> Result<AuthPhase, AuthError> {
        if key.is_empty() {
            return Err(AuthError::failed("API key is empty"));
        }
        if !Self::login_method_allowed(&provider, LoginMethod::ApiKey) {
            return Err(AuthError::failed(format!(
                "API key login is not allowed for {}",
                provider.as_str()
            )));
        }
        let phase = self
            .logins
            .lock()
            .get(&login_id)
            .map(|login| (login.provider.clone(), login.phase.clone()));
        let Some((session_provider, session_phase)) = phase else {
            return Err(AuthError::NoActiveLogin);
        };
        if session_provider != provider {
            return Err(AuthError::failed("login belongs to a different provider"));
        }
        if !matches!(session_phase, AuthPhase::AwaitingApiKey { .. }) {
            return Err(AuthError::failed("login is not awaiting an API key"));
        }
        let endpoint = self.endpoint_for(&provider)?;
        let models = validate_api_key(&self.http, &provider, &endpoint, &key)?;
        self.runtime
            .store
            .set(&provider, StoredCredential::api_key(key))?;
        let public = PublicAuthRecord {
            email: None,
            account_id: None,
            expires_at_ms: None,
            relogin_required: false,
        };
        if let Err(error) = self.write_public(&provider, public) {
            let _ = self.runtime.store.delete(&provider);
            return Err(error);
        }
        let cache_key = AuthPersist::catalog_cache_key(&provider, &endpoint);
        if self
            .runtime
            .persist
            .put_catalog_at(
                &cache_key,
                &provider,
                self.enrich_catalog(&provider, models),
                CatalogSource::Live,
                self.now(),
            )
            .is_err()
        {
            let _ = self.runtime.store.delete(&provider);
            let mut accounts = self.runtime.persist.load_accounts();
            accounts.accounts.remove(provider.as_str());
            let _ = self.runtime.persist.save_accounts(&accounts);
            return Err(AuthError::Store);
        }
        let completed = AuthPhase::Completed {
            login_id,
            provider: provider.clone(),
        };
        if let Some(login) = self.logins.lock().get_mut(&login_id) {
            login.phase = completed.clone();
        }
        Ok(completed)
    }

    pub fn cancel_login(&self, login_id: Uuid) -> Result<(), AuthError> {
        self.shutdown_callback(login_id);
        self.logins
            .lock()
            .remove(&login_id)
            .ok_or(AuthError::NoActiveLogin)
            .map(|_| ())
    }

    pub fn logout(&self, provider: &ProviderId) -> Result<(), AuthError> {
        let ids: Vec<Uuid> = self
            .logins
            .lock()
            .iter()
            .filter(|(_, login)| login.provider == *provider)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let _ = self.cancel_login(id);
        }
        self.runtime.store.delete(provider)?;
        let mut accounts = self.runtime.persist.load_accounts();
        accounts.accounts.remove(provider.as_str());
        self.runtime
            .persist
            .save_accounts(&accounts)
            .map_err(|_| AuthError::Store)
    }

    pub fn resolve(
        &self,
        provider: &ProviderId,
        endpoint: &ExternalProvider,
    ) -> Result<(Auth, Vec<(String, String)>), AuthError> {
        if self
            .runtime
            .persist
            .load_accounts()
            .accounts
            .get(provider.as_str())
            .is_some_and(|record| record.relogin_required)
        {
            return Err(AuthError::ReloginRequired(provider.clone()));
        }
        if let Some(stored) = self.refresh_if_due(provider)? {
            let extra = match (&stored, ProviderPreset::parse_id(provider.as_str())) {
                (
                    StoredCredential::Oauth {
                        access, account_id, ..
                    },
                    Some(ProviderPreset::OpenAiCodex),
                ) => account_id
                    .clone()
                    .or_else(|| super::jwt::chatgpt_account_id(access))
                    .map(|id| vec![("chatgpt-account-id".into(), id)])
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            return Ok((
                stored_to_auth(
                    ProviderPreset::parse_id(provider.as_str()),
                    &stored,
                    endpoint.api_format,
                ),
                extra,
            ));
        }
        if let Some(preset) = ProviderPreset::parse_id(provider.as_str())
            && let Some(env) = preset.env_key()
            && let Ok(value) = std::env::var(env)
            && !value.trim().is_empty()
        {
            let auth = match endpoint.api_format {
                wakuwaku_protocol::ApiFormat::Anthropic => Auth::AnthropicApiKey {
                    key: value,
                    version: "2023-06-01".into(),
                },
                _ => Auth::Bearer(value),
            };
            return Ok((auth, Vec::new()));
        }
        // Custom endpoints without a stored credential run unauthenticated —
        // local gateways (LM Studio, Ollama, llama.cpp) accept that. Presets
        // still require a credential or their documented environment key.
        if ProviderPreset::parse_id(provider.as_str()).is_none() {
            return Ok((Auth::None, Vec::new()));
        }
        Err(AuthError::failed(format!(
            "provider {} is not configured",
            provider
        )))
    }

    fn refresh_if_due(&self, provider: &ProviderId) -> Result<Option<StoredCredential>, AuthError> {
        let _guard = self.refresh_lock.lock();
        let Some(stored) = self.runtime.store.get(provider)? else {
            return Ok(None);
        };
        let StoredCredential::Oauth {
            refresh,
            expires_at_ms,
            account_id,
            email,
            ..
        } = &stored
        else {
            return Ok(Some(stored));
        };
        if self.now() + 30_000 < *expires_at_ms {
            return Ok(Some(stored));
        }
        let tokens = match ProviderPreset::parse_id(provider.as_str()) {
            Some(ProviderPreset::OpenAiCodex) => {
                flows::refresh_codex_token(&self.http, &self.runtime.endpoints, refresh, self.now())
            }
            Some(ProviderPreset::XaiOauth) => {
                flows::refresh_xai_token(&self.http, &self.runtime.endpoints, refresh, self.now())
            }
            _ => return Ok(Some(stored)),
        };
        match tokens {
            Ok(mut tokens) => {
                if tokens.account_id.is_none() {
                    tokens.account_id = account_id.clone();
                }
                if tokens.email.is_none() {
                    tokens.email = email.clone();
                }
                let stored = StoredCredential::Oauth {
                    access: tokens.access,
                    refresh: tokens.refresh,
                    expires_at_ms: tokens.expires_at_ms,
                    account_id: tokens.account_id.clone(),
                    email: tokens.email.clone(),
                };
                self.runtime.store.set(provider, stored.clone())?;
                self.write_public(
                    provider,
                    PublicAuthRecord {
                        email: tokens.email,
                        account_id: tokens.account_id,
                        expires_at_ms: Some(tokens.expires_at_ms),
                        relogin_required: false,
                    },
                )?;
                Ok(Some(stored))
            }
            Err(error) if error.to_string().contains("invalid_grant") => {
                self.write_public(
                    provider,
                    PublicAuthRecord {
                        email: email.clone(),
                        account_id: account_id.clone(),
                        expires_at_ms: Some(*expires_at_ms),
                        relogin_required: true,
                    },
                )?;
                Err(AuthError::ReloginRequired(provider.clone()))
            }
            Err(error) => Err(error),
        }
    }

    pub fn refresh_models(&self, provider: &ProviderId) -> Result<ModelCatalog, AuthError> {
        let endpoint = self.endpoint_for(provider)?;
        let cache_key = AuthPersist::catalog_cache_key(provider, &endpoint);
        if ProviderPreset::parse_id(provider.as_str()) == Some(ProviderPreset::XaiOauth)
            && self.runtime.store.get(provider)?.is_none()
        {
            return self.persist_xai_oauth_seed(provider, &endpoint, &cache_key);
        }
        let (auth, extra) = self.resolve(provider, &endpoint)?;
        let transport = ProviderPreset::parse_id(provider.as_str())
            .map(ProviderPreset::transport)
            .unwrap_or(TransportProfile::Standard);
        match flows::fetch_models(&self.http, provider, &endpoint, auth, transport, extra) {
            Ok(models) => self
                .runtime
                .persist
                .put_catalog_at(
                    &cache_key,
                    provider,
                    self.enrich_catalog(provider, models),
                    CatalogSource::Live,
                    self.now(),
                )
                .map_err(|_| AuthError::Store),
            Err(error) => {
                if let Some(cached) = self
                    .runtime
                    .persist
                    .get_catalog(&cache_key)
                    .or_else(|| self.runtime.persist.get_catalog(provider.as_str()))
                {
                    return Ok(ModelCatalog {
                        source: CatalogSource::Cache,
                        ..cached
                    });
                }
                if ProviderPreset::parse_id(provider.as_str()) == Some(ProviderPreset::XaiOauth) {
                    return self.persist_xai_oauth_seed(provider, &endpoint, &cache_key);
                }
                Err(error)
            }
        }
    }

    fn persist_xai_oauth_seed(
        &self,
        provider: &ProviderId,
        endpoint: &ExternalProvider,
        cache_key: &str,
    ) -> Result<ModelCatalog, AuthError> {
        self.runtime
            .persist
            .put_catalog_at(
                cache_key,
                provider,
                xai_oauth_seed(&endpoint.base_url),
                CatalogSource::Seed,
                self.now(),
            )
            .map_err(|_| AuthError::Store)
    }

    fn enrich_catalog(
        &self,
        provider: &ProviderId,
        models: Vec<ModelCatalogEntry>,
    ) -> Vec<ModelCatalogEntry> {
        let Some(url) = self.runtime.models_dev_url.as_deref() else {
            return models;
        };
        let Some(preset) = ProviderPreset::parse_id(provider.as_str()) else {
            return models;
        };
        flows::enrich_models(&self.models_dev_http, url, preset, models)
    }

    fn cached_catalog(&self, provider: &ProviderId) -> Option<ModelCatalog> {
        let endpoint = self.endpoint_for(provider).ok()?;
        let cache_key = AuthPersist::catalog_cache_key(provider, &endpoint);
        self.runtime
            .persist
            .get_catalog(&cache_key)
            .or_else(|| self.runtime.persist.get_catalog(provider.as_str()))
    }

    pub fn resolve_reasoning_effort(
        &self,
        provider: &ProviderId,
        model: Option<&str>,
        provider_value: Option<&str>,
    ) -> Option<(String, String)> {
        let selected = provider_value?.trim();
        if selected.is_empty() {
            return None;
        }
        let catalog = self.cached_catalog(provider)?;
        let entry = select_entry(&catalog, model)?;
        if !entry.supported || !entry.capabilities.reasoning_effort {
            return None;
        }
        entry.reasoning_efforts.iter().find_map(|effort| {
            (effort.id == selected || effort.provider_value == selected)
                .then(|| (effort.id.clone(), effort.provider_value.clone()))
        })
    }

    pub fn list_models(&self, provider: &ProviderId) -> Result<ModelCatalog, AuthError> {
        let endpoint = self.endpoint_for(provider)?;
        let cache_key = AuthPersist::catalog_cache_key(provider, &endpoint);
        if let Some(cached) = self
            .runtime
            .persist
            .get_catalog(&cache_key)
            .or_else(|| self.runtime.persist.get_catalog(provider.as_str()))
        {
            return Ok(cached);
        }
        self.refresh_models(provider)
    }

    pub fn overlay_for_model(
        &self,
        provider: &ProviderId,
        model: Option<&str>,
    ) -> Result<ResolvedProviderOverlay, AuthError> {
        let endpoint = self.endpoint_for(provider)?;
        let preset = ProviderPreset::parse_id(provider.as_str());
        let selected = model.map(str::trim).filter(|value| !value.is_empty());
        let using_default = selected.is_none();
        let catalog = if preset.is_some() {
            Some(self.list_models(provider))
        } else {
            None
        };
        if let Some(preset) = preset {
            let catalog = match catalog.expect("preset catalog") {
                Ok(catalog) => catalog,
                Err(_) if using_default => {
                    let capabilities = capabilities_for_preset(preset, endpoint.api_format);
                    let (auth, extra) = self.resolve(provider, &endpoint)?;
                    return Ok((
                        endpoint,
                        preset.transport(),
                        auth,
                        extra,
                        capabilities,
                        ProviderLimits::default(),
                    ));
                }
                Err(error) => return Err(error),
            };
            if let Some(entry) = select_entry(&catalog, selected) {
                return apply_catalog_entry(self, provider, endpoint, entry);
            }
            if using_default {
                let capabilities = capabilities_for_preset(preset, endpoint.api_format);
                let (auth, extra) = self.resolve(provider, &endpoint)?;
                return Ok((
                    endpoint,
                    preset.transport(),
                    auth,
                    extra,
                    capabilities,
                    ProviderLimits::default(),
                ));
            }
            return Err(AuthError::failed(format!(
                "model {} is not in the catalog",
                selected.unwrap_or_default()
            )));
        }
        if let Some(catalog) = self.cached_catalog(provider)
            && let Some(entry) = select_entry(&catalog, selected)
        {
            return apply_catalog_entry(self, provider, endpoint, entry);
        }
        let (auth, extra) = self.resolve(provider, &endpoint)?;
        Ok((
            endpoint.clone(),
            TransportProfile::Standard,
            auth,
            extra,
            ModelCapabilities::custom(endpoint.api_format),
            ProviderLimits::default(),
        ))
    }

    fn endpoint_for(&self, provider: &ProviderId) -> Result<ExternalProvider, AuthError> {
        if let Some(preset) = ProviderPreset::parse_id(provider.as_str()) {
            let mut endpoint = preset.endpoint();
            if let Some(base) = self.runtime.model_base_overrides.get(provider.as_str()) {
                endpoint.base_url = base.clone();
            }
            return Ok(endpoint);
        }
        if let Some(custom) = self
            .customs
            .lock()
            .iter()
            .find(|candidate| candidate.id == *provider)
            .cloned()
        {
            return Ok(custom);
        }
        Err(AuthError::failed(format!(
            "provider {} is not configured",
            provider
        )))
    }

    fn store_oauth(
        &self,
        provider: &ProviderId,
        tokens: flows::OauthTokens,
    ) -> Result<(), AuthError> {
        self.runtime.store.set(
            provider,
            StoredCredential::Oauth {
                access: tokens.access,
                refresh: tokens.refresh,
                expires_at_ms: tokens.expires_at_ms,
                account_id: tokens.account_id.clone(),
                email: tokens.email.clone(),
            },
        )?;
        self.write_public(
            provider,
            PublicAuthRecord {
                email: tokens.email,
                account_id: tokens.account_id,
                expires_at_ms: Some(tokens.expires_at_ms),
                relogin_required: false,
            },
        )
    }

    fn write_public(
        &self,
        provider: &ProviderId,
        record: PublicAuthRecord,
    ) -> Result<(), AuthError> {
        let mut accounts = self.runtime.persist.load_accounts();
        accounts
            .accounts
            .insert(provider.as_str().to_owned(), record);
        self.runtime
            .persist
            .save_accounts(&accounts)
            .map_err(|_| AuthError::Store)
    }
}

fn apply_catalog_entry(
    service: &AuthService,
    provider: &ProviderId,
    mut endpoint: ExternalProvider,
    entry: &ModelCatalogEntry,
) -> Result<ResolvedProviderOverlay, AuthError> {
    if !entry.supported {
        return Err(AuthError::failed(
            entry
                .unsupported_reason
                .map(|reason| format!("model {} is {}", entry.id, reason.as_str()))
                .unwrap_or_else(|| format!("model {} is unsupported", entry.id)),
        ));
    }
    endpoint.base_url = entry.base_url.clone();
    endpoint.api_format = entry.api_format;
    let (auth, extra) = service.resolve(provider, &endpoint)?;
    Ok((
        endpoint,
        entry.transport,
        auth,
        extra,
        entry.capabilities,
        entry.limits(),
    ))
}

fn capabilities_for_preset(
    preset: ProviderPreset,
    format: wakuwaku_protocol::ApiFormat,
) -> ModelCapabilities {
    match preset {
        ProviderPreset::OpenAiResponses | ProviderPreset::OpenAiChat => {
            ModelCapabilities::openai_api(format)
        }
        ProviderPreset::OpenAiCodex => ModelCapabilities::codex(),
        ProviderPreset::Xai | ProviderPreset::XaiOauth => ModelCapabilities::xai(true),
        ProviderPreset::Anthropic => ModelCapabilities::anthropic(),
        ProviderPreset::OpenCodeZen | ProviderPreset::OpenCodeGo => {
            ModelCapabilities::openai_compatible(format)
        }
    }
}

fn select_entry<'a>(
    catalog: &'a ModelCatalog,
    model: Option<&str>,
) -> Option<&'a ModelCatalogEntry> {
    let requested = model.map(str::trim).filter(|value| !value.is_empty());
    if let Some(requested) = requested {
        return catalog.models.iter().find(|entry| entry.id == requested);
    }
    catalog
        .models
        .iter()
        .find(|entry| entry.supported)
        .or_else(|| catalog.models.first())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::{MemoryCredentialStore, StoredCredential};
    use wakuwaku_protocol::ModelCapabilities;

    #[test]
    fn env_fallback_does_not_cross_alias_xai() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-auth-{}", Uuid::new_v4()));
        let service = AuthService::new(AuthRuntime::testing(
            &directory,
            Arc::new(MemoryCredentialStore::default()),
            AuthEndpoints::production(),
        ))
        .unwrap();
        unsafe {
            std::env::set_var("XAI_API_KEY", "xai-only");
            std::env::remove_var("XAI_OAUTH_TOKEN");
        }
        let statuses = service.status(None);
        let xai = statuses
            .iter()
            .find(|status| status.provider.as_str() == "xai")
            .unwrap();
        let oauth = statuses
            .iter()
            .find(|status| status.provider.as_str() == "xai-oauth")
            .unwrap();
        assert_eq!(xai.method, AuthMethod::EnvKey);
        assert_eq!(oauth.method, AuthMethod::None);
        unsafe {
            std::env::remove_var("XAI_API_KEY");
        }
    }

    fn bind_json(status: u16, body: serde_json::Value) -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);
                let payload = body.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        port
    }

    fn service_with_store(
        directory: &std::path::Path,
        store: Arc<MemoryCredentialStore>,
    ) -> AuthService {
        AuthService::new(AuthRuntime::testing(
            directory,
            store,
            AuthEndpoints::production(),
        ))
        .unwrap()
    }

    #[test]
    fn go_and_zen_stored_keys_never_cross() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-gozen-{}", Uuid::new_v4()));
        let store = Arc::new(MemoryCredentialStore::default());
        store
            .set(
                &ProviderId::new("opencode-zen"),
                StoredCredential::api_key(SecretString::new("zen-only")),
            )
            .unwrap();
        let service = service_with_store(&directory, store);
        unsafe {
            std::env::remove_var("OPENCODE_API_KEY");
        }
        let zen = ProviderPreset::OpenCodeZen.endpoint();
        let go = ProviderPreset::OpenCodeGo.endpoint();
        let (zen_auth, _) = service
            .resolve(&ProviderId::new("opencode-zen"), &zen)
            .unwrap();
        assert!(matches!(zen_auth, Auth::Bearer(key) if key == "zen-only"));
        let go_auth = service.resolve(&ProviderId::new("opencode-go"), &go);
        assert!(go_auth.is_err() || matches!(go_auth, Ok((Auth::None, _))));
    }

    #[test]
    fn transport_failure_keeps_last_good_catalog() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-cache-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(MemoryCredentialStore::default());
        let provider = ProviderId::new("xai");
        store
            .set(&provider, StoredCredential::api_key(SecretString::new("k")))
            .unwrap();
        let persist = AuthPersist::new(&directory);
        persist
            .put_catalog(
                &provider,
                vec![ModelCatalogEntry {
                    provider: provider.clone(),
                    id: "grok-4.5".into(),
                    name: "Grok".into(),
                    api_format: wakuwaku_protocol::ApiFormat::OpenAiChat,
                    transport: TransportProfile::Standard,
                    base_url: "https://api.x.ai/v1".into(),
                    context_window: 128_000,
                    max_output_tokens: 8_192,
                    reasoning: true,
                    reasoning_efforts: vec![
                        wakuwaku_protocol::ReasoningEffortOption {
                            id: "low".into(),
                            provider_value: "low".into(),
                            label: "Low".into(),
                        },
                        wakuwaku_protocol::ReasoningEffortOption {
                            id: "medium".into(),
                            provider_value: "balanced".into(),
                            label: "Balanced".into(),
                        },
                        wakuwaku_protocol::ReasoningEffortOption {
                            id: "high".into(),
                            provider_value: "high".into(),
                            label: "High".into(),
                        },
                    ],
                    default_reasoning_effort: Some("high".into()),
                    capabilities: ModelCapabilities::xai(true),
                    supported: true,
                    unsupported_reason: None,
                }],
                CatalogSource::Live,
                1,
            )
            .unwrap();

        let mut runtime = AuthRuntime::testing(&directory, store, AuthEndpoints::production());
        runtime
            .model_base_overrides
            .insert("xai".into(), "http://127.0.0.1:1/v1".into());
        let service = AuthService::new(runtime).unwrap();
        assert_eq!(
            service.resolve_reasoning_effort(&provider, Some("grok-4.5"), Some("medium")),
            Some(("medium".into(), "balanced".into()))
        );
        assert_eq!(
            service.resolve_reasoning_effort(&provider, Some("grok-4.5"), Some("stale")),
            None
        );
        let catalog = service.refresh_models(&provider).unwrap();
        assert_eq!(catalog.source, CatalogSource::Cache);
        assert_eq!(catalog.models[0].id, "grok-4.5");
    }

    #[test]
    fn xai_oauth_live_discovery_error_falls_back_to_seed() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-xai-seed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(MemoryCredentialStore::default());
        let provider = ProviderId::new("xai-oauth");
        store
            .set(
                &provider,
                StoredCredential::Oauth {
                    access: "access".into(),
                    refresh: "refresh".into(),
                    expires_at_ms: u64::MAX,
                    account_id: None,
                    email: None,
                },
            )
            .unwrap();
        let mut runtime = AuthRuntime::testing(&directory, store, AuthEndpoints::production());
        runtime
            .model_base_overrides
            .insert("xai-oauth".into(), "http://127.0.0.1:1/v1".into());
        let service = AuthService::new(runtime).unwrap();
        let catalog = service.refresh_models(&provider).unwrap();
        assert_eq!(catalog.source, CatalogSource::Seed);
        assert!(
            catalog
                .models
                .iter()
                .any(|model| model.id == "grok-4.5" && model.supported)
        );
    }

    #[test]
    fn empty_models_http_200_is_live_empty() {
        let port = bind_json(200, serde_json::json!({ "data": [] }));
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-empty-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(MemoryCredentialStore::default());
        let provider = ProviderId::new("xai");
        store
            .set(&provider, StoredCredential::api_key(SecretString::new("k")))
            .unwrap();
        let mut runtime = AuthRuntime::testing(&directory, store, AuthEndpoints::production());
        runtime
            .model_base_overrides
            .insert("xai".into(), format!("http://127.0.0.1:{port}/v1"));
        let service = AuthService::new(runtime).unwrap();
        let catalog = service.refresh_models(&provider).unwrap();
        assert_eq!(catalog.source, CatalogSource::Live);
        assert!(catalog.models.is_empty());
    }

    #[test]
    fn openai_default_overlay_without_catalog_has_no_priority() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-no-catalog-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(MemoryCredentialStore::default());
        let provider = ProviderId::new(ProviderId::OPENAI_RESPONSES);
        store
            .set(
                &provider,
                StoredCredential::api_key(SecretString::new("sk")),
            )
            .unwrap();
        let mut runtime = AuthRuntime::testing(&directory, store, AuthEndpoints::production());
        runtime
            .model_base_overrides
            .insert(provider.as_str().into(), "http://127.0.0.1:1/v1".into());
        let service = AuthService::new(runtime).unwrap();
        let overlay = service.overlay_for_model(&provider, None).unwrap();
        assert!(!overlay.4.service_tier);
        assert!(
            !capabilities_for_preset(
                ProviderPreset::OpenAiResponses,
                wakuwaku_protocol::ApiFormat::OpenAiResponses
            )
            .service_tier
        );
        assert!(
            !capabilities_for_preset(
                ProviderPreset::OpenAiChat,
                wakuwaku_protocol::ApiFormat::OpenAiChat
            )
            .service_tier
        );
    }

    #[test]
    fn injected_models_dev_fixture_enriches_live_openai_catalog() {
        let models_port = bind_json(200, serde_json::json!({ "data": [{ "id": "gpt-5.5" }] }));
        let meta_port = bind_json(
            200,
            serde_json::json!({
                "openai": {
                    "models": {
                        "gpt-5.5": {
                            "reasoning_options": [{
                                "type": "effort",
                                "values": ["low", "high"]
                            }],
                            "experimental": {
                                "modes": {
                                    "fast": {
                                        "provider": { "body": { "service_tier": "priority" } }
                                    }
                                }
                            }
                        }
                    }
                }
            }),
        );
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-models-dev-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(MemoryCredentialStore::default());
        let provider = ProviderId::new(ProviderId::OPENAI_RESPONSES);
        store
            .set(
                &provider,
                StoredCredential::api_key(SecretString::new("sk")),
            )
            .unwrap();
        let mut runtime = AuthRuntime::testing(&directory, store, AuthEndpoints::production());
        runtime.model_base_overrides.insert(
            provider.as_str().into(),
            format!("http://127.0.0.1:{models_port}/v1"),
        );
        runtime.models_dev_url = Some(format!("http://127.0.0.1:{meta_port}/api.json"));
        let service = AuthService::new(runtime).unwrap();
        let catalog = service.refresh_models(&provider).unwrap();
        assert_eq!(catalog.source, CatalogSource::Live);
        assert_eq!(catalog.models[0].id, "gpt-5.5");
        assert!(catalog.models[0].capabilities.service_tier);
        assert_eq!(
            catalog.models[0]
                .reasoning_efforts
                .iter()
                .map(|effort| effort.provider_value.as_str())
                .collect::<Vec<_>>(),
            ["low", "high"]
        );
        assert!(catalog.models[0].default_reasoning_effort.is_none());
    }

    #[test]
    fn api_key_login_never_writes_settings_json() {
        let port = bind_json(200, serde_json::json!({ "data": [{ "id": "grok-4.5" }] }));
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-settings-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut runtime = AuthRuntime::testing(
            &directory,
            Arc::new(MemoryCredentialStore::default()),
            AuthEndpoints::production(),
        );
        runtime
            .model_base_overrides
            .insert("xai".into(), format!("http://127.0.0.1:{port}/v1"));
        let service = AuthService::new(runtime).unwrap();
        let AuthPhase::AwaitingApiKey { login_id, .. } = service
            .start_login(ProviderId::new("xai"), LoginMethod::ApiKey)
            .unwrap()
        else {
            panic!("expected api key phase");
        };
        service
            .complete_api_key(
                login_id,
                ProviderId::new("xai"),
                SecretString::new("sk-never-disk"),
            )
            .unwrap();
        assert!(!directory.join("settings.json").exists());
        let auth_status = std::fs::read_to_string(directory.join("auth-status.json")).unwrap();
        assert!(!auth_status.contains("sk-never-disk"));
    }

    #[test]
    fn browser_callback_is_accepted_between_status_polls() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-auth-cb-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(MemoryCredentialStore::default());
        let service = service_with_store(&directory, store);
        let phase = service
            .start_login(ProviderId::new("openai-codex"), LoginMethod::OauthBrowser)
            .unwrap();
        let AuthPhase::AwaitingBrowser { login_id, .. } = phase else {
            panic!("{phase:?}");
        };
        let addr = service.browser_callback_addr(login_id).expect("listener");
        let state = {
            let logins = service.logins.lock();
            logins
                .get(&login_id)
                .and_then(|login| login.state.clone())
                .unwrap()
        };
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        let request = format!(
            "GET /auth/callback?code=abc&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
        );
        use std::io::Write;
        stream.write_all(request.as_bytes()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let captured = loop {
            let received = service
                .callbacks
                .lock()
                .get(&login_id)
                .and_then(|callback| callback.received.lock().clone());
            if received.is_some() || std::time::Instant::now() >= deadline {
                break received;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let Some((code, captured_state)) = captured else {
            panic!("callback was not captured");
        };
        assert_eq!(code, "abc");
        assert_eq!(captured_state, state);
    }

    #[test]
    fn busy_callback_port_returns_device_fallback() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-auth-busy-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = occupied.local_addr().unwrap();
        let store = Arc::new(MemoryCredentialStore::default());
        let mut runtime = AuthRuntime::testing(&directory, store, AuthEndpoints::production());
        runtime.callback_bind = addr;
        let service = AuthService::new(runtime).unwrap();
        let phase = service
            .start_login(ProviderId::new("openai-codex"), LoginMethod::OauthBrowser)
            .unwrap();
        match phase {
            AuthPhase::Failed {
                login_id,
                provider,
                message,
            } => {
                assert!(message.contains("device login"), "{message}");
                assert_eq!(provider.as_str(), "openai-codex");
                assert_eq!(
                    service.auth_phases(Some(&ProviderId::new("openai-codex")))[0].login_id(),
                    Some(login_id)
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn selected_go_model_missing_from_catalog_is_an_error() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-auth-go-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(MemoryCredentialStore::default());
        store
            .set(
                &ProviderId::new("opencode-go"),
                StoredCredential::api_key(SecretString::new("go-key")),
            )
            .unwrap();
        let persist = AuthPersist::new(&directory);
        persist
            .put_catalog(
                &ProviderId::new("opencode-go"),
                vec![ModelCatalogEntry {
                    provider: ProviderId::new("opencode-go"),
                    id: "kimi-k2.7-code".into(),
                    name: "Kimi".into(),
                    api_format: wakuwaku_protocol::ApiFormat::OpenAiResponses,
                    transport: TransportProfile::Standard,
                    base_url: "https://opencode.ai/zen/go/v1".into(),
                    context_window: 128_000,
                    max_output_tokens: 8_192,
                    reasoning: false,
                    reasoning_efforts: Vec::new(),
                    default_reasoning_effort: None,
                    capabilities: ModelCapabilities::openai_compatible(
                        wakuwaku_protocol::ApiFormat::OpenAiResponses,
                    ),
                    supported: true,
                    unsupported_reason: None,
                }],
                CatalogSource::Live,
                1,
            )
            .unwrap();
        let service = service_with_store(&directory, store);
        let error = service
            .overlay_for_model(&ProviderId::new("opencode-go"), Some("missing-model"))
            .unwrap_err();
        assert!(error.to_string().contains("not in the catalog"), "{error}");
    }

    #[test]
    fn unsupported_catalog_entry_never_resolves() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-unsup-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(MemoryCredentialStore::default());
        store
            .set(
                &ProviderId::new("opencode-zen"),
                StoredCredential::api_key(SecretString::new("zen-key")),
            )
            .unwrap();
        let persist = AuthPersist::new(&directory);
        persist
            .put_catalog(
                &ProviderId::new("opencode-zen"),
                vec![ModelCatalogEntry {
                    provider: ProviderId::new("opencode-zen"),
                    id: "gemini-3-flash".into(),
                    name: "Gemini".into(),
                    api_format: wakuwaku_protocol::ApiFormat::OpenAiChat,
                    transport: TransportProfile::Standard,
                    base_url: "https://opencode.ai/zen/v1".into(),
                    context_window: 128_000,
                    max_output_tokens: 8_192,
                    reasoning: false,
                    reasoning_efforts: Vec::new(),
                    default_reasoning_effort: None,
                    capabilities: ModelCapabilities::openai_compatible(
                        wakuwaku_protocol::ApiFormat::OpenAiChat,
                    ),
                    supported: false,
                    unsupported_reason: Some(wakuwaku_protocol::UnsupportedReason::GoogleFormat),
                }],
                CatalogSource::Live,
                1,
            )
            .unwrap();
        let service = service_with_store(&directory, store);
        let error = service
            .overlay_for_model(&ProviderId::new("opencode-zen"), Some("gemini-3-flash"))
            .unwrap_err();
        assert!(
            error.to_string().contains("unsupported") || error.to_string().contains("google"),
            "{error}"
        );
    }

    #[test]
    fn api_key_is_rejected_for_codex_and_supergrok() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-method-{}", Uuid::new_v4()));
        let service = service_with_store(&directory, Arc::new(MemoryCredentialStore::default()));
        assert!(
            service
                .start_login(ProviderId::new("openai-codex"), LoginMethod::ApiKey)
                .is_err()
        );
        assert!(
            service
                .start_login(ProviderId::new("xai-oauth"), LoginMethod::ApiKey)
                .is_err()
        );
        assert!(
            service
                .start_login(ProviderId::new("xai"), LoginMethod::OauthDevice)
                .is_err()
        );
    }

    #[test]
    fn complete_api_key_requires_matching_awaiting_session() {
        let port = bind_json(200, serde_json::json!({ "data": [{ "id": "grok-4.5" }] }));
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-phase-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut runtime = AuthRuntime::testing(
            &directory,
            Arc::new(MemoryCredentialStore::default()),
            AuthEndpoints::production(),
        );
        runtime
            .model_base_overrides
            .insert("xai".into(), format!("http://127.0.0.1:{port}/v1"));
        let service = AuthService::new(runtime).unwrap();
        let AuthPhase::AwaitingApiKey { login_id, .. } = service
            .start_login(ProviderId::new("xai"), LoginMethod::ApiKey)
            .unwrap()
        else {
            panic!("awaiting");
        };
        assert!(
            service
                .complete_api_key(
                    login_id,
                    ProviderId::new("anthropic"),
                    SecretString::new("x")
                )
                .is_err()
        );
        service.cancel_login(login_id).unwrap();
        assert!(
            service
                .complete_api_key(login_id, ProviderId::new("xai"), SecretString::new("x"))
                .is_err()
        );
        let AuthPhase::AwaitingApiKey { login_id, .. } = service
            .start_login(ProviderId::new("xai"), LoginMethod::ApiKey)
            .unwrap()
        else {
            panic!("awaiting");
        };
        service
            .complete_api_key(login_id, ProviderId::new("xai"), SecretString::new("sk"))
            .unwrap();
        let replay =
            service.complete_api_key(login_id, ProviderId::new("xai"), SecretString::new("sk"));
        assert!(replay.is_err(), "{replay:?}");
    }

    #[test]
    fn device_first_poll_honors_interval_on_controllable_clock() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-clock-{}", Uuid::new_v4()));
        let service = service_with_store(&directory, Arc::new(MemoryCredentialStore::default()));
        service.set_clock_ms(1_000);
        assert_eq!(
            AuthService::first_poll_delay_ms(&ProviderId::new("openai-codex"), 5_000),
            5_000
        );
        assert_eq!(
            AuthService::first_poll_delay_ms(&ProviderId::new("openai-codex"), 1_000),
            4_000
        );
        assert_eq!(
            AuthService::first_poll_delay_ms(&ProviderId::new("xai-oauth"), 3_000),
            3_000
        );
    }

    #[test]
    fn supergrok_oauth_does_not_add_chatgpt_account_header() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-auth-hdr-{}", Uuid::new_v4()));
        let store = Arc::new(MemoryCredentialStore::default());
        store
            .set(
                &ProviderId::new("xai-oauth"),
                StoredCredential::Oauth {
                    access: "tok".into(),
                    refresh: "ref".into(),
                    expires_at_ms: u64::MAX,
                    account_id: Some("acct".into()),
                    email: None,
                },
            )
            .unwrap();
        let service = service_with_store(&directory, store);
        let endpoint = ProviderPreset::XaiOauth.endpoint();
        let (_, extra) = service
            .resolve(&ProviderId::new("xai-oauth"), &endpoint)
            .unwrap();
        assert!(extra.iter().all(|(name, _)| name != "chatgpt-account-id"));
    }

    #[test]
    fn concurrent_provider_logins_stay_self_describing() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-auth-concurrent-{}", Uuid::new_v4()));
        let service = service_with_store(&directory, Arc::new(MemoryCredentialStore::default()));
        let xai = service
            .start_login(ProviderId::new("xai"), LoginMethod::ApiKey)
            .unwrap();
        let anthropic = service
            .start_login(ProviderId::new("anthropic"), LoginMethod::ApiKey)
            .unwrap();
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

        let all = service.auth_phases(None);
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(
            |phase| phase.provider().map(ProviderId::as_str) == Some("xai")
                && phase.login_id() == Some(xai_id)
        ));
        assert!(all.iter().any(|phase| {
            phase.provider().map(ProviderId::as_str) == Some("anthropic")
                && phase.login_id() == Some(anthropic_id)
        }));

        let only_xai = service.auth_phases(Some(&ProviderId::new("xai")));
        assert_eq!(only_xai.len(), 1);
        assert_eq!(only_xai[0].provider().map(ProviderId::as_str), Some("xai"));
        assert_eq!(only_xai[0].login_id(), Some(xai_id));

        service.cancel_login(xai_id).unwrap();
        let remaining = service.auth_phases(None);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].login_id(), Some(anthropic_id));
        assert_eq!(
            remaining[0].provider().map(ProviderId::as_str),
            Some("anthropic")
        );
        assert!(
            service
                .auth_phases(Some(&ProviderId::new("xai")))
                .is_empty()
        );
    }
}
