//! Canonical provider identity and validated endpoint configuration.
//!
//! `base_url` is the API root and includes `/v1`. Request paths are joined
//! with `url::Url::join`, never concatenated.

mod auth_wire;
mod catalog;
mod endpoints;
mod models_dev;
mod preset;
mod secret;
mod transport;

pub use auth_wire::{AuthMethod, AuthPhase, LoginMethod, ModelCatalog, ProviderAuthStatus};
pub use catalog::{
    CatalogError, ModelCatalogEntry, OpenCodeRoute, ReasoningEffortOption, apply_opencode_route,
    apply_xai_policy, is_grok_reasoning_effort_capable, is_openai_non_chat_model,
    is_xai_non_chat_model, parse_anthropic_models_envelope, parse_codex_models_envelope,
    parse_openai_models_envelope, route_opencode_model, xai_oauth_seed,
};
pub use endpoints::{AuthEndpoints, is_pinned_xai_token_endpoint};
pub use models_dev::{
    MODELS_DEV_API_URL, ModelsDevCatalog, enrich_catalog_from_models_dev, models_dev_source_key,
    parse_models_dev_document,
};
pub use preset::{PresetAuthKind, ProviderPreset};
pub use secret::SecretString;
pub use transport::{CatalogSource, ModelCapabilities, TransportProfile, UnsupportedReason};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use ts_rs::TS;
use url::Url;

/// Stable, owned endpoint identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct ProviderId(String);

impl Serialize for ProviderId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

impl ProviderId {
    pub const OPENAI_RESPONSES: &'static str = "openai-responses";
    pub const OPENAI_CHAT: &'static str = "openai-chat";
    pub const ANTHROPIC: &'static str = "anthropic";
    pub const OPENAI_CODEX: &'static str = "openai-codex";
    pub const OPENCODE_ZEN: &'static str = "opencode-zen";
    pub const OPENCODE_GO: &'static str = "opencode-go";
    pub const XAI: &'static str = "xai";
    pub const XAI_OAUTH: &'static str = "xai-oauth";
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_valid(&self) -> bool {
        let value = self.0.trim();
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    }

    pub fn validate(&self) -> Result<(), String> {
        self.is_valid()
            .then_some(())
            .ok_or_else(|| format!("invalid provider id {:?}", self.as_str()))
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl AsRef<str> for ProviderId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Closed set of HTTP wire protocols.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ApiFormat {
    #[default]
    OpenAiResponses,
    OpenAiChat,
    Anthropic,
}

impl ApiFormat {
    pub const ALL: [Self; 3] = [Self::OpenAiResponses, Self::OpenAiChat, Self::Anthropic];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "openai-responses",
            Self::OpenAiChat => "openai-chat",
            Self::Anthropic => "anthropic",
        }
    }

    /// Relative path joined onto a `/v1` API root.
    pub const fn route_segment(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "responses",
            Self::OpenAiChat => "chat/completions",
            Self::Anthropic => "messages",
        }
    }
}

impl fmt::Display for ApiFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// OpenAI-native request priority (`auto`/`default`/`flex`/`priority`).
///
/// Whether a selected model accepts this field is
/// [`crate::ModelCapabilities::service_tier`] on the chosen catalog entry,
/// not the HTTP dialect.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Auto,
    Default,
    Flex,
    Priority,
}

impl ServiceTier {
    pub const ALL: [Self; 4] = [Self::Auto, Self::Default, Self::Flex, Self::Priority];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Default => "default",
            Self::Flex => "flex",
            Self::Priority => "priority",
        }
    }
}

impl fmt::Display for ServiceTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Resource limits for one endpoint, expressed in tokens.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProviderLimits {
    pub context_window: u64,
    pub max_output_tokens: u64,
}

impl Default for ProviderLimits {
    fn default() -> Self {
        Self {
            context_window: 128_000,
            max_output_tokens: 16_384,
        }
    }
}

impl ProviderLimits {
    pub fn validate(self) -> Result<(), String> {
        if self.context_window == 0 || self.max_output_tokens == 0 {
            return Err("provider limits must be positive".to_owned());
        }
        if self.max_output_tokens > self.context_window {
            return Err("provider output limit cannot exceed its context window".to_owned());
        }
        Ok(())
    }
}

/// Serializable endpoint configuration. It intentionally has no secret value:
/// API keys live in the OS credential store, and model limits arrive with the
/// discovered catalog entry rather than being configured per endpoint.
///
/// The model list is never stored here either: models are discovered from the
/// endpoint's standard `/models` interface and cached as a catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ExternalProvider {
    pub id: ProviderId,
    pub name: String,
    pub base_url: String,
    pub api_format: ApiFormat,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
}

impl ExternalProvider {
    pub fn new(
        id: impl Into<ProviderId>,
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_format: ApiFormat,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            base_url: base_url.into(),
            api_format,
            headers: Vec::new(),
        }
    }

    pub fn standard_defaults() -> Vec<Self> {
        vec![
            Self::new(
                ProviderId::OPENAI_RESPONSES,
                "OpenAI Responses",
                "https://api.openai.com/v1",
                ApiFormat::OpenAiResponses,
            ),
            Self::new(
                ProviderId::OPENAI_CHAT,
                "OpenAI Chat",
                "https://api.openai.com/v1",
                ApiFormat::OpenAiChat,
            ),
            Self::new(
                ProviderId::ANTHROPIC,
                "Anthropic",
                "https://api.anthropic.com/v1",
                ApiFormat::Anthropic,
            ),
        ]
    }

    pub fn request_url(&self) -> Result<String, String> {
        let mut root = parse_api_root(&self.base_url)?;
        if !root.path().ends_with('/') {
            root.set_path(&format!("{}/", root.path()));
        }
        let path = self.api_format.route_segment();
        root.join(path)
            .map(|url| url.to_string())
            .map_err(|_| format!("provider {} cannot join route {path}", self.id))
    }

    pub fn validate(&self) -> Result<(), String> {
        self.id.validate()?;
        if self.name.trim().is_empty() {
            return Err("provider name must not be empty".to_owned());
        }
        parse_api_root(&self.base_url)?;
        let mut seen_headers = std::collections::HashSet::new();
        for (name, value) in &self.headers {
            let lowered = name.to_ascii_lowercase();
            if name.trim().is_empty()
                || name
                    .as_bytes()
                    .iter()
                    .any(|byte| byte.is_ascii_whitespace() || *byte == b':')
            {
                return Err(format!("invalid header name: {name}"));
            }
            if !seen_headers.insert(lowered.clone()) {
                return Err(format!("duplicate header name: {name}"));
            }
            if is_reserved_header(&lowered) {
                return Err(format!(
                    "header is reserved and cannot override transport/auth state: {name}"
                ));
            }
            if value.contains('\r') || value.contains('\n') {
                return Err(format!("invalid value for header: {name}"));
            }
        }
        Ok(())
    }
}

fn parse_api_root(base_url: &str) -> Result<Url, String> {
    let url = Url::parse(base_url.trim())
        .map_err(|_| "provider base URL must be a valid http(s) URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("provider base URL must be a valid http(s) URL".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("provider base URL must not include credentials".to_owned());
    }
    if url.query().is_some() {
        return Err("provider base URL must not include a query string".to_owned());
    }
    if url.fragment().is_some() {
        return Err("provider base URL must not include a fragment".to_owned());
    }
    Ok(url)
}

fn is_reserved_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "x-api-key"
            | "anthropic-version"
            | "host"
            | "content-length"
            | "content-type"
    )
}

/// Execution credential. Never serialized; Debug redacts secret material.
#[derive(Clone)]
pub enum Auth {
    Bearer(String),
    AnthropicApiKey { key: String, version: String },
    None,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Auth::AnthropicApiKey { version, .. } => {
                write!(f, "AnthropicApiKey(<redacted>, version={version})")
            }
            Auth::None => f.write_str("None"),
        }
    }
}

impl Auth {
    pub fn secrets(&self) -> impl Iterator<Item = &str> {
        match self {
            Auth::Bearer(secret) | Auth::AnthropicApiKey { key: secret, .. } => {
                Some(secret.as_str()).into_iter()
            }
            Auth::None => None.into_iter(),
        }
    }

    pub fn validate_for_format(&self, format: ApiFormat) -> Result<(), String> {
        match (format, self) {
            (ApiFormat::OpenAiResponses | ApiFormat::OpenAiChat, Auth::AnthropicApiKey { .. }) => {
                Err("anthropic key auth cannot be used with an OpenAI-format endpoint".into())
            }
            (ApiFormat::Anthropic, Auth::AnthropicApiKey { key, version })
                if key.trim().is_empty() || version.trim().is_empty() =>
            {
                Err("anthropic auth is missing a key or version".into())
            }
            (_, Auth::Bearer(key)) if key.trim().is_empty() => Err("bearer auth is empty".into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_owned_and_defaults_carry_only_endpoint_facts() {
        let providers = ExternalProvider::standard_defaults();
        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].id.as_str(), ProviderId::OPENAI_RESPONSES);
        assert_eq!(providers[0].base_url, "https://api.openai.com/v1");
        assert_eq!(providers[2].base_url, "https://api.anthropic.com/v1");
        let json = serde_json::to_string(&providers).unwrap();
        assert!(json.contains("apiFormat"));
        assert!(!json.contains("sk-"));
        assert!(
            !json.contains("apiKeyEnv"),
            "endpoint config must not name credentials"
        );
    }

    #[test]
    fn request_url_joins_v1_root_without_doubling() {
        let openai = ExternalProvider::standard_defaults()
            .into_iter()
            .find(|p| p.api_format == ApiFormat::OpenAiResponses)
            .unwrap();
        assert_eq!(
            openai.request_url().unwrap(),
            "https://api.openai.com/v1/responses"
        );
        let chat = ExternalProvider::standard_defaults()
            .into_iter()
            .find(|p| p.api_format == ApiFormat::OpenAiChat)
            .unwrap();
        assert_eq!(
            chat.request_url().unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
        let anthropic = ExternalProvider::standard_defaults()
            .into_iter()
            .find(|p| p.api_format == ApiFormat::Anthropic)
            .unwrap();
        assert_eq!(
            anthropic.request_url().unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn validate_rejects_credentials_query_fragment_and_reserved_headers() {
        let mut p = ExternalProvider::new(
            "local",
            "Local",
            "https://api.openai.com/v1",
            ApiFormat::OpenAiResponses,
        );
        p.base_url = "https://user:pass@api.openai.com/v1".into();
        assert!(p.validate().unwrap_err().contains("credentials"));
        p.base_url = "https://api.openai.com/v1?x=1".into();
        assert!(p.validate().unwrap_err().contains("query"));
        p.base_url = "https://api.openai.com/v1#frag".into();
        assert!(p.validate().unwrap_err().contains("fragment"));
        p.base_url = "https://api.openai.com/v1".into();
        p.headers = vec![("anthropic-version".into(), "2023-06-01".into())];
        assert!(p.validate().unwrap_err().contains("reserved"));
    }

    #[test]
    fn provider_id_rejects_path_like_values() {
        assert!(!ProviderId::new("../secret").is_valid());
        assert!(ProviderId::new("local-gateway").is_valid());
    }

    #[test]
    fn provider_id_wire_json_is_a_bare_string() {
        let id = ProviderId::new("openai-responses");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"openai-responses\"");
        let parsed: ProviderId = serde_json::from_str("\"openai-chat\"").unwrap();
        assert_eq!(parsed.as_str(), "openai-chat");
        let object = serde_json::json!({ "provider": id });
        assert_eq!(object["provider"], "openai-responses");
    }

    #[test]
    fn anthropic_auth_is_rejected_for_openai_format() {
        let err = Auth::AnthropicApiKey {
            key: "k".into(),
            version: "2023-06-01".into(),
        }
        .validate_for_format(ApiFormat::OpenAiChat)
        .unwrap_err();
        assert!(err.contains("anthropic key auth"));
    }

    #[test]
    fn service_tier_wire_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&ServiceTier::Flex).unwrap(),
            "\"flex\""
        );
        assert_eq!(
            serde_json::from_str::<ServiceTier>("\"priority\"").unwrap(),
            ServiceTier::Priority
        );
    }
}
