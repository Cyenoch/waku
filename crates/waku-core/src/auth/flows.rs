//! Login, refresh, and model-list HTTP. Endpoints are injected.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::Value;
use waku_harness::{Auth, auth_headers, parse_models_payload};
use waku_protocol::{
    ApiFormat, AuthEndpoints, ExternalProvider, ModelCatalogEntry, ProviderId, ProviderPreset,
    SecretString, TransportProfile,
};

use super::error::AuthError;
use super::jwt;
use super::store::StoredCredential;

const TOKEN_TIMEOUT: Duration = Duration::from_secs(20);
const ACCESS_SKEW_MS: u64 = 5 * 60 * 1000;
#[derive(Debug)]
pub struct OauthTokens {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: u64,
    pub account_id: Option<String>,
    pub email: Option<String>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub fn http_client() -> Result<Client, AuthError> {
    Client::builder()
        .timeout(TOKEN_TIMEOUT)
        .build()
        .map_err(|_| AuthError::failed("could not build HTTP client"))
}

pub fn exchange_codex_code(
    http: &Client,
    endpoints: &AuthEndpoints,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    now_ms: u64,
) -> Result<OauthTokens, AuthError> {
    let mut form = HashMap::new();
    form.insert("grant_type", "authorization_code");
    form.insert("client_id", AuthEndpoints::CODEX_CLIENT_ID);
    form.insert("code", code);
    form.insert("code_verifier", verifier);
    form.insert("redirect_uri", redirect_uri);
    let response = http
        .post(&endpoints.openai_token)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&form)
        .send()
        .map_err(|_| AuthError::failed("Codex token exchange failed"))?;
    if !response.status().is_success() {
        return Err(AuthError::failed(format!(
            "Codex token exchange failed: {}",
            response.status()
        )));
    }
    parse_codex_token(
        response
            .json()
            .map_err(|_| AuthError::failed("invalid token JSON"))?,
        now_ms,
    )
}

pub fn refresh_codex_token(
    http: &Client,
    endpoints: &AuthEndpoints,
    refresh: &str,
    now_ms: u64,
) -> Result<OauthTokens, AuthError> {
    let mut form = HashMap::new();
    form.insert("grant_type", "refresh_token");
    form.insert("client_id", AuthEndpoints::CODEX_CLIENT_ID);
    form.insert("refresh_token", refresh);
    let response = http
        .post(&endpoints.openai_token)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&form)
        .send()
        .map_err(|_| AuthError::failed("Codex refresh failed"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if body.contains("invalid_grant") {
            return Err(AuthError::failed("invalid_grant"));
        }
        return Err(AuthError::failed(format!("Codex refresh failed: {status}")));
    }
    parse_codex_token(
        response
            .json()
            .map_err(|_| AuthError::failed("invalid token JSON"))?,
        now_ms,
    )
}

fn parse_codex_token(payload: Value, now_ms: u64) -> Result<OauthTokens, AuthError> {
    let access = payload
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::failed("token response missing access_token"))?;
    let refresh = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::failed("token response missing refresh_token"))?;
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or_else(|| AuthError::failed("token response missing expires_in"))?;
    let account_id = jwt::chatgpt_account_id(access)
        .ok_or_else(|| AuthError::failed("failed to extract accountId from token"))?;
    let email = payload
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt::decode_payload)
        .and_then(|claims| {
            claims
                .get("email")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    Ok(OauthTokens {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires_at_ms: now_ms.saturating_add(expires_in.saturating_mul(1000)),
        account_id: Some(account_id),
        email,
    })
}

pub struct CodexDeviceStart {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval_ms: u64,
}

pub fn start_codex_device(
    http: &Client,
    endpoints: &AuthEndpoints,
) -> Result<CodexDeviceStart, AuthError> {
    let response = http
        .post(&endpoints.openai_device_usercode)
        .json(&serde_json::json!({ "client_id": AuthEndpoints::CODEX_CLIENT_ID }))
        .send()
        .map_err(|_| AuthError::failed("Codex device authorization failed"))?;
    if !response.status().is_success() {
        return Err(AuthError::failed("Codex device authorization failed"));
    }
    let payload: Value = response
        .json()
        .map_err(|_| AuthError::failed("invalid device JSON"))?;
    let device_auth_id = payload
        .get("device_auth_id")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::failed("device response missing device_auth_id"))?;
    let user_code = payload
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::failed("device response missing user_code"))?;
    let interval = payload
        .get("interval")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .unwrap_or(5);
    Ok(CodexDeviceStart {
        device_auth_id: device_auth_id.to_owned(),
        user_code: user_code.to_owned(),
        interval_ms: interval.saturating_mul(1000),
    })
}
#[derive(Debug)]
pub enum DevicePoll<T> {
    Pending,
    SlowDown,
    Complete(T),
}

pub fn poll_codex_device(
    http: &Client,
    endpoints: &AuthEndpoints,
    device_auth_id: &str,
    user_code: &str,
    now_ms: u64,
) -> Result<DevicePoll<OauthTokens>, AuthError> {
    let response = http
        .post(&endpoints.openai_device_token)
        .json(&serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        }))
        .send()
        .map_err(|_| AuthError::failed("Codex device poll failed"))?;
    if matches!(response.status().as_u16(), 403 | 404) {
        return Ok(DevicePoll::Pending);
    }
    if !response.status().is_success() {
        return Err(AuthError::failed("Codex device poll failed"));
    }
    let payload: Value = response
        .json()
        .map_err(|_| AuthError::failed("invalid device token JSON"))?;
    let code = payload
        .get("authorization_code")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::failed("device token missing authorization_code"))?;
    let verifier = payload
        .get("code_verifier")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::failed("device token missing code_verifier"))?;
    exchange_codex_code(
        http,
        endpoints,
        code,
        verifier,
        &endpoints.openai_device_redirect,
        now_ms,
    )
    .map(DevicePoll::Complete)
}

pub struct XaiDeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval_ms: u64,
    pub expires_in_ms: u64,
    pub token_endpoint: String,
}

pub fn start_xai_device(
    http: &Client,
    endpoints: &AuthEndpoints,
) -> Result<XaiDeviceStart, AuthError> {
    let discovery: Value = http
        .get(&endpoints.xai_discovery)
        .header(ACCEPT, "application/json")
        .send()
        .and_then(|response| response.error_for_status()?.json())
        .map_err(|_| AuthError::failed("xAI OIDC discovery failed"))?;
    let token_endpoint = discovery
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::failed("xAI discovery missing token_endpoint"))?;
    if !endpoints.allows_xai_token_endpoint(token_endpoint) {
        return Err(AuthError::failed(
            "xAI token_endpoint is not a pinned x.ai host",
        ));
    }
    let response = http
        .post(&endpoints.xai_device_code)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ACCEPT, "application/json")
        .form(&[
            ("client_id", AuthEndpoints::XAI_OAUTH_CLIENT_ID),
            ("scope", AuthEndpoints::XAI_OAUTH_SCOPE),
        ])
        .send()
        .map_err(|_| AuthError::failed("xAI device-code request failed"))?;
    if !response.status().is_success() {
        return Err(AuthError::failed("xAI device-code request failed"));
    }
    let payload: Value = response
        .json()
        .map_err(|_| AuthError::failed("invalid xAI device JSON"))?;
    let device_code = required_str(&payload, "device_code")?;
    let user_code = required_str(&payload, "user_code")?;
    let verification = required_str(&payload, "verification_uri_complete")?;
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| AuthError::failed("xAI device-code missing expires_in"))?;
    let interval = payload
        .get("interval")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| AuthError::failed("xAI device-code missing interval"))?;
    Ok(XaiDeviceStart {
        device_code: device_code.to_owned(),
        user_code: user_code.to_owned(),
        verification_url: verification.to_owned(),
        interval_ms: (interval * 1000.0) as u64,
        expires_in_ms: (expires_in * 1000.0) as u64,
        token_endpoint: token_endpoint.to_owned(),
    })
}

pub fn poll_xai_device(
    http: &Client,
    endpoints: &AuthEndpoints,
    token_endpoint: &str,
    device_code: &str,
    now_ms: u64,
) -> Result<DevicePoll<OauthTokens>, AuthError> {
    if !endpoints.allows_xai_token_endpoint(token_endpoint) {
        return Err(AuthError::failed(
            "xAI token_endpoint is not a pinned x.ai host",
        ));
    }
    let response = http
        .post(token_endpoint)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ACCEPT, "application/json")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", AuthEndpoints::XAI_OAUTH_CLIENT_ID),
            ("device_code", device_code),
        ])
        .send()
        .map_err(|_| AuthError::failed("xAI device poll failed"))?;
    let status = response.status();
    let payload: Value = response
        .json()
        .map_err(|_| AuthError::failed("invalid xAI poll JSON"))?;
    if status.is_success() {
        return parse_xai_token(payload, None, now_ms).map(DevicePoll::Complete);
    }
    match payload.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Ok(DevicePoll::Pending),
        Some("slow_down") => Ok(DevicePoll::SlowDown),
        other => Err(AuthError::failed(format!(
            "xAI device poll failed: {}",
            other.unwrap_or("unknown")
        ))),
    }
}

pub fn refresh_xai_token(
    http: &Client,
    endpoints: &AuthEndpoints,
    refresh: &str,
    now_ms: u64,
) -> Result<OauthTokens, AuthError> {
    let discovery: Value = http
        .get(&endpoints.xai_discovery)
        .header(ACCEPT, "application/json")
        .send()
        .and_then(|response| response.error_for_status()?.json())
        .map_err(|_| AuthError::failed("xAI OIDC discovery failed"))?;
    let token_endpoint = discovery
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::failed("xAI discovery missing token_endpoint"))?;
    if !endpoints.allows_xai_token_endpoint(token_endpoint) {
        return Err(AuthError::failed(
            "xAI token_endpoint is not a pinned x.ai host",
        ));
    }
    let response = http
        .post(token_endpoint)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ACCEPT, "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", AuthEndpoints::XAI_OAUTH_CLIENT_ID),
            ("refresh_token", refresh),
        ])
        .send()
        .map_err(|_| AuthError::failed("xAI refresh failed"))?;
    if !response.status().is_success() {
        let body = response.text().unwrap_or_default();
        if body.contains("invalid_grant") {
            return Err(AuthError::failed("invalid_grant"));
        }
        return Err(AuthError::failed("xAI refresh failed"));
    }
    parse_xai_token(
        response
            .json()
            .map_err(|_| AuthError::failed("invalid xAI refresh JSON"))?,
        Some(refresh),
        now_ms,
    )
}

fn parse_xai_token(
    payload: Value,
    refresh_fallback: Option<&str>,
    now_ms: u64,
) -> Result<OauthTokens, AuthError> {
    let access = required_str(&payload, "access_token")?;
    let refresh = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or(refresh_fallback)
        .ok_or_else(|| AuthError::failed("xAI token missing refresh_token"))?;
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| AuthError::failed("xAI token missing expires_in"))?;
    Ok(OauthTokens {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires_at_ms: now_ms
            .saturating_add((expires_in * 1000.0) as u64)
            .saturating_sub(ACCESS_SKEW_MS),
        account_id: None,
        email: None,
    })
}

fn required_str<'a>(payload: &'a Value, field: &str) -> Result<&'a str, AuthError> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::failed(format!("missing {field}")))
}

pub fn fetch_models(
    http: &Client,
    provider: &ProviderId,
    endpoint: &ExternalProvider,
    auth: Auth,
    transport: TransportProfile,
    extra_auth_headers: Vec<(String, String)>,
) -> Result<Vec<ModelCatalogEntry>, AuthError> {
    let config = waku_harness::ProviderConfig {
        endpoint: endpoint.clone(),
        auth,
        transport,
        extra_auth_headers,
    };
    let preset = ProviderPreset::parse_id(provider.as_str());
    let url = waku_harness::models_url_for(preset, &endpoint.base_url)
        .map_err(|error| AuthError::failed(error.to_string()))?;
    let headers = auth_headers(&config).map_err(|error| AuthError::failed(error.to_string()))?;
    let response = http
        .get(url)
        .headers(headers)
        .send()
        .map_err(|_| AuthError::failed("model list request failed"))?;
    if !response.status().is_success() {
        return Err(AuthError::failed(format!(
            "model list failed: {}",
            response.status()
        )));
    }
    let payload: Value = response
        .json()
        .map_err(|_| AuthError::failed("model list was not JSON"))?;
    parse_models_payload(
        preset,
        &payload,
        provider.clone(),
        &endpoint.base_url,
        endpoint.api_format,
        transport,
    )
    .map_err(|error| AuthError::failed(error.to_string()))
}

pub fn validate_api_key(
    http: &Client,
    provider: &ProviderId,
    endpoint: &ExternalProvider,
    key: &SecretString,
) -> Result<Vec<ModelCatalogEntry>, AuthError> {
    let format = endpoint.api_format;
    let auth = match format {
        ApiFormat::Anthropic => Auth::AnthropicApiKey {
            key: key.expose().to_owned(),
            version: "2023-06-01".into(),
        },
        ApiFormat::OpenAiResponses | ApiFormat::OpenAiChat => Auth::Bearer(key.expose().to_owned()),
    };
    fetch_models(
        http,
        provider,
        endpoint,
        auth,
        TransportProfile::Standard,
        Vec::new(),
    )
}

pub fn stored_to_auth(
    _preset: Option<ProviderPreset>,
    stored: &StoredCredential,
    format: ApiFormat,
) -> Auth {
    match stored {
        StoredCredential::ApiKey { key } => match format {
            ApiFormat::Anthropic => Auth::AnthropicApiKey {
                key: key.clone(),
                version: "2023-06-01".into(),
            },
            _ => Auth::Bearer(key.clone()),
        },
        StoredCredential::Oauth { access, .. } => Auth::Bearer(access.clone()),
    }
}

pub fn codex_authorize_url(endpoints: &AuthEndpoints, state: &str, challenge: &str) -> String {
    let mut url = reqwest::Url::parse(&endpoints.openai_authorize).unwrap_or_else(|_| {
        reqwest::Url::parse("https://auth.openai.com/oauth/authorize").expect("static")
    });
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", AuthEndpoints::CODEX_CLIENT_ID)
        .append_pair("redirect_uri", &AuthEndpoints::codex_callback_uri())
        .append_pair("scope", AuthEndpoints::CODEX_SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", AuthEndpoints::CODEX_ORIGINATOR);
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unpinned_xai_token_endpoint_is_rejected() {
        let http = http_client().unwrap();
        let err = poll_xai_device(
            &http,
            &AuthEndpoints::production(),
            "https://evil.example/token",
            "device",
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("pinned"));
    }

    #[test]
    fn codex_token_without_account_id_is_rejected() {
        let err = parse_codex_token(
            json!({
                "access_token": "aaa.e30.sig",
                "refresh_token": "refresh",
                "expires_in": 3600
            }),
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("accountId"));
    }

    #[test]
    fn codex_authorize_url_pins_loopback_1455() {
        let url = codex_authorize_url(&AuthEndpoints::production(), "state", "challenge");
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(!url.contains("127.0.0.1"));
    }

    #[test]
    fn zen_anthropic_key_uses_x_api_key_not_bearer() {
        let auth = stored_to_auth(
            Some(ProviderPreset::OpenCodeZen),
            &StoredCredential::api_key(SecretString::new("zen-key")),
            ApiFormat::Anthropic,
        );
        assert!(matches!(auth, Auth::AnthropicApiKey { key, .. } if key == "zen-key"));
    }
}
