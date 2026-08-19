//! Provider seam: HTTP endpoint configuration, auth, and registry.
//!
//! A provider here is an HTTP endpoint (not a CLI program). The registry is
//! one of the two dyn seams in the crate; `Harness` dispatches on the closed
//! `ApiFormat` enum and never on strings.

use crate::error::HarnessError;
use crate::model::ApiFormat;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

pub use wakuwaku_provider::Auth;

/// Static extra headers merged after auth headers.
pub type ExtraHeaders = Vec<(String, String)>;

/// Resolved execution handle: canonical endpoint plus secret auth.
#[derive(Clone)]
pub struct ProviderConfig {
    pub endpoint: wakuwaku_provider::ExternalProvider,
    /// Token budget for requests against this endpoint, from the selected
    /// catalog entry (or the default when no catalog informed the session).
    pub limits: wakuwaku_provider::ProviderLimits,
    pub auth: Auth,
    pub transport: wakuwaku_provider::TransportProfile,
    pub extra_auth_headers: ExtraHeaders,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("endpoint", &self.endpoint)
            .field("limits", &self.limits)
            .field("auth", &self.auth)
            .field("transport", &self.transport)
            .field("extra_auth_headers", &"<redacted-names-only>")
            .finish()
    }
}

impl ProviderConfig {
    pub fn new(endpoint: wakuwaku_provider::ExternalProvider, auth: Auth) -> Self {
        Self {
            endpoint,
            limits: wakuwaku_provider::ProviderLimits::default(),
            auth,
            transport: wakuwaku_provider::TransportProfile::Standard,
            extra_auth_headers: Vec::new(),
        }
    }
}

/// Normalized outbound request produced by the registry.
pub struct ProviderRequest {
    pub url: String,
    pub method: reqwest::Method,
    pub headers: reqwest::header::HeaderMap,
    pub body: Value,
}

/// Registry of configured providers; resolves auth per request.
#[derive(Clone, Debug, Default)]
pub struct Providers {
    providers: Arc<Mutex<Vec<ProviderConfig>>>,
    http: reqwest::Client,
}

impl Providers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_client(http: reqwest::Client) -> Self {
        Providers {
            providers: Arc::new(Mutex::new(Vec::new())),
            http,
        }
    }

    /// Replace the set of providers wholesale after validating the complete
    /// registry. Keeping validation here means every request sees the same
    /// invariants, regardless of which caller loaded the settings.
    pub fn set_providers(&self, providers: Vec<ProviderConfig>) -> Result<(), HarnessError> {
        let mut ids = HashSet::with_capacity(providers.len());
        for provider in &providers {
            validate_provider(provider)?;
            if !ids.insert(provider.endpoint.id.as_str()) {
                return Err(HarnessError::InvalidRequest(format!(
                    "duplicate provider id: {}",
                    provider.endpoint.id
                )));
            }
        }
        *self.providers.lock() = providers;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<ProviderConfig> {
        self.providers
            .lock()
            .iter()
            .find(|p| p.endpoint.id.as_str() == id)
            .cloned()
    }

    pub fn list(&self) -> Vec<ProviderConfig> {
        self.providers.lock().clone()
    }

    pub fn replace_auth(
        &self,
        id: &str,
        auth: Auth,
        extra_auth_headers: ExtraHeaders,
    ) -> Result<(), HarnessError> {
        let mut providers = self.providers.lock();
        let provider = providers
            .iter_mut()
            .find(|p| p.endpoint.id.as_str() == id)
            .ok_or_else(|| HarnessError::UnknownProvider(id.to_owned()))?;
        provider.auth = auth;
        provider.extra_auth_headers = extra_auth_headers;
        validate_provider(provider)
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Resolve the model id for a request. Endpoint config carries no default
    /// model — models come from the endpoint's catalog — so callers must name
    /// one explicitly.
    pub fn resolve_model(
        &self,
        provider_id: &str,
        model: Option<&str>,
    ) -> Result<(ProviderConfig, String), HarnessError> {
        let provider = self
            .get(provider_id)
            .ok_or_else(|| HarnessError::UnknownProvider(provider_id.to_string()))?;
        let model_id = model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                HarnessError::InvalidRequest(format!(
                    "provider {provider_id} requires an explicit catalog model"
                ))
            })?;
        Ok((provider, model_id))
    }

    /// Build the outbound request for a provider, applying auth headers.
    pub fn build_request(
        &self,
        provider: &ProviderConfig,
        body: Value,
    ) -> Result<ProviderRequest, HarnessError> {
        validate_provider(provider)?;
        let mut headers = route_and_auth_headers(provider)?;
        for (name, value) in &provider.endpoint.headers {
            let header_name =
                reqwest::header::HeaderName::try_from(name.as_str()).map_err(|_| {
                    HarnessError::InvalidRequest(format!("invalid header name: {name}"))
                })?;
            let header_value =
                reqwest::header::HeaderValue::try_from(value.as_str()).map_err(|_| {
                    HarnessError::InvalidRequest(format!("invalid value for header: {name}"))
                })?;
            if is_reserved_header(&header_name) {
                return Err(HarnessError::InvalidRequest(format!(
                    "header is reserved and cannot override transport/auth state: {name}"
                )));
            }
            headers.append(header_name, header_value);
        }
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        Ok(ProviderRequest {
            url: provider
                .endpoint
                .request_url()
                .map_err(HarnessError::InvalidRequest)?,
            method: reqwest::Method::POST,
            headers,
            body,
        })
    }
}

fn validate_provider(provider: &ProviderConfig) -> Result<(), HarnessError> {
    provider
        .endpoint
        .validate()
        .map_err(HarnessError::InvalidRequest)?;
    provider
        .limits
        .validate()
        .map_err(HarnessError::InvalidRequest)?;
    provider
        .auth
        .validate_for_format(provider.endpoint.api_format)
        .map_err(HarnessError::InvalidRequest)?;
    Ok(())
}

fn is_reserved_header(name: &reqwest::header::HeaderName) -> bool {
    matches!(
        name.as_str().to_ascii_lowercase().as_str(),
        "authorization"
            | "x-api-key"
            | "anthropic-version"
            | "host"
            | "content-length"
            | "content-type"
    )
}

fn route_and_auth_headers(
    provider: &ProviderConfig,
) -> Result<reqwest::header::HeaderMap, HarnessError> {
    let mut headers = reqwest::header::HeaderMap::new();
    match provider.endpoint.api_format {
        ApiFormat::OpenAiResponses | ApiFormat::OpenAiChat => {
            apply_bearer(&mut headers, provider)?;
        }
        ApiFormat::Anthropic => match &provider.auth {
            Auth::AnthropicApiKey { key, version } => {
                headers.insert("x-api-key", header_value(key, "x-api-key")?);
                headers.insert(
                    "anthropic-version",
                    header_value(version, "anthropic-version")?,
                );
            }
            Auth::Bearer(key) => {
                let bearer = format!("Bearer {key}");
                headers.insert("authorization", header_value(&bearer, "authorization")?);
            }
            Auth::None => {}
        },
    }
    if provider.transport == wakuwaku_provider::TransportProfile::Codex {
        headers.insert(
            "openai-beta",
            header_value("responses=experimental", "openai-beta")?,
        );
        headers.insert(
            "originator",
            header_value(
                wakuwaku_provider::AuthEndpoints::CODEX_ORIGINATOR,
                "originator",
            )?,
        );
        headers.insert(
            "version",
            header_value(
                wakuwaku_provider::AuthEndpoints::client_version(),
                "version",
            )?,
        );
    }
    for (name, value) in &provider.extra_auth_headers {
        let header_name = reqwest::header::HeaderName::try_from(name.as_str())
            .map_err(|_| HarnessError::InvalidRequest(format!("invalid header name: {name}")))?;
        headers.insert(header_name, header_value(value, name)?);
    }
    Ok(headers)
}

fn apply_bearer(
    headers: &mut reqwest::header::HeaderMap,
    provider: &ProviderConfig,
) -> Result<(), HarnessError> {
    match &provider.auth {
        Auth::Bearer(key) => {
            let bearer = format!("Bearer {key}");
            headers.insert("authorization", header_value(&bearer, "authorization")?);
            Ok(())
        }
        Auth::None => Ok(()),
        Auth::AnthropicApiKey { .. } => {
            // Anthropic key auth against an OpenAI-format endpoint is a
            // configuration error; reject rather than sending a wrong header.
            Err(HarnessError::NotConfigured {
                provider: provider.endpoint.id.as_str().to_owned(),
            })
        }
    }
}

fn header_value(v: &str, field: &str) -> Result<reqwest::header::HeaderValue, HarnessError> {
    reqwest::header::HeaderValue::try_from(v)
        .map_err(|_| HarnessError::InvalidRequest(format!("invalid value for header: {field}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(format: ApiFormat, auth: Auth) -> ProviderConfig {
        ProviderConfig {
            endpoint: wakuwaku_provider::ExternalProvider {
                id: wakuwaku_provider::ProviderId::new("p"),
                name: "P".into(),
                base_url: "https://example.test/v1".into(),
                api_format: format,
                headers: vec![("x-extra".into(), "1".into())],
            },
            limits: wakuwaku_provider::ProviderLimits {
                context_window: 100_000,
                max_output_tokens: 8_192,
            },
            auth,
            transport: wakuwaku_provider::TransportProfile::Standard,
            extra_auth_headers: Vec::new(),
        }
    }

    #[test]
    fn bearer_auth_sets_authorization_header() {
        let p = provider(ApiFormat::OpenAiResponses, Auth::Bearer("secret".into()));
        let providers = Providers::new();
        providers.set_providers(vec![p.clone()]).unwrap();
        let req = providers.build_request(&p, serde_json::json!({})).unwrap();
        assert_eq!(req.url, "https://example.test/v1/responses");
        assert_eq!(req.headers.get("authorization").unwrap(), "Bearer secret");
        assert_eq!(req.headers.get("x-extra").unwrap(), "1");
    }

    #[test]
    fn anthropic_auth_sets_key_and_version() {
        let p = provider(
            ApiFormat::Anthropic,
            Auth::AnthropicApiKey {
                key: "k".into(),
                version: "2023-06-01".into(),
            },
        );
        let providers = Providers::new();
        providers.set_providers(vec![p.clone()]).unwrap();
        let req = providers.build_request(&p, serde_json::json!({})).unwrap();
        assert_eq!(req.url, "https://example.test/v1/messages");
        assert_eq!(req.headers.get("x-api-key").unwrap(), "k");
        assert_eq!(req.headers.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn auth_debug_never_leaks_keys() {
        let a = Auth::Bearer("super-secret".into());
        assert!(!format!("{a:?}").contains("super-secret"));
        let b = Auth::AnthropicApiKey {
            key: "super-secret".into(),
            version: "v".into(),
        };
        assert!(!format!("{b:?}").contains("super-secret"));
        let config = provider(
            ApiFormat::OpenAiResponses,
            Auth::Bearer("super-secret".into()),
        );
        assert!(!format!("{config:?}").contains("super-secret"));
    }

    #[test]
    fn set_providers_rejects_duplicate_ids_and_invalid_limits() {
        let providers = Providers::new();
        let mut duplicate = provider(ApiFormat::OpenAiResponses, Auth::None);
        duplicate.endpoint.name = "Other".into();
        assert!(
            providers
                .set_providers(vec![
                    provider(ApiFormat::OpenAiResponses, Auth::None),
                    duplicate,
                ])
                .is_err()
        );

        let mut invalid = provider(ApiFormat::OpenAiResponses, Auth::None);
        invalid.limits.max_output_tokens = 0;
        assert!(providers.set_providers(vec![invalid]).is_err());
    }

    #[test]
    fn set_providers_rejects_reserved_or_malformed_headers() {
        let providers = Providers::new();
        let mut reserved = provider(ApiFormat::OpenAiResponses, Auth::None);
        reserved.endpoint.headers = vec![("Authorization".into(), "override".into())];
        assert!(providers.set_providers(vec![reserved]).is_err());

        let mut malformed = provider(ApiFormat::OpenAiResponses, Auth::None);
        malformed.endpoint.headers = vec![("not valid".into(), "value".into())];
        assert!(providers.set_providers(vec![malformed]).is_err());
    }

    #[test]
    fn codex_transport_sets_subscription_headers() {
        let mut p = provider(ApiFormat::OpenAiResponses, Auth::Bearer("tok".into()));
        p.transport = wakuwaku_provider::TransportProfile::Codex;
        p.extra_auth_headers = vec![("chatgpt-account-id".into(), "acct_1".into())];
        p.endpoint.base_url = "https://chatgpt.com/backend-api/codex".into();
        let providers = Providers::new();
        providers.set_providers(vec![p.clone()]).unwrap();
        let req = providers.build_request(&p, serde_json::json!({})).unwrap();
        assert_eq!(req.url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(
            req.headers.get("openai-beta").unwrap(),
            "responses=experimental"
        );
        assert_eq!(req.headers.get("originator").unwrap(), "waku");
        assert_eq!(
            req.headers.get("version").unwrap(),
            wakuwaku_provider::AuthEndpoints::client_version()
        );
        assert_eq!(req.headers.get("chatgpt-account-id").unwrap(), "acct_1");
    }
}
