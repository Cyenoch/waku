//! Live model catalog fetch against a configured HTTP endpoint.

use crate::error::HarnessError;
use crate::provider::{Auth, ProviderConfig};
use serde_json::Value;
use wakuwaku_provider::{
    CatalogError, ModelCatalogEntry, ProviderPreset, TransportProfile, apply_opencode_route,
    apply_xai_policy, parse_anthropic_models_envelope, parse_codex_models_envelope,
    parse_openai_models_envelope,
};

pub fn models_url(base_url: &str) -> Result<String, HarnessError> {
    models_url_for(None, base_url)
}

pub fn models_url_for(
    preset: Option<ProviderPreset>,
    base_url: &str,
) -> Result<String, HarnessError> {
    let mut root = url::Url::parse(base_url.trim()).map_err(|_| {
        HarnessError::InvalidRequest("provider base URL must be a valid http(s) URL".into())
    })?;
    if !root.path().ends_with('/') {
        root.set_path(&format!("{}/", root.path()));
    }
    let mut url = root
        .join("models")
        .map_err(|_| HarnessError::InvalidRequest("provider cannot join /models".into()))?;
    if preset == Some(ProviderPreset::OpenAiCodex) {
        url.query_pairs_mut().append_pair(
            "client_version",
            wakuwaku_provider::AuthEndpoints::client_version(),
        );
    }
    Ok(url.into())
}

pub fn parse_models_payload(
    preset: Option<ProviderPreset>,
    payload: &Value,
    provider: wakuwaku_provider::ProviderId,
    base_url: &str,
    format: wakuwaku_provider::ApiFormat,
    transport: TransportProfile,
) -> Result<Vec<ModelCatalogEntry>, CatalogError> {
    let models = match preset {
        Some(ProviderPreset::OpenAiCodex) => {
            parse_codex_models_envelope(payload, provider, base_url)?
        }
        Some(ProviderPreset::Anthropic) => {
            parse_anthropic_models_envelope(payload, provider, base_url)?
        }
        Some(ProviderPreset::OpenCodeZen | ProviderPreset::OpenCodeGo) => {
            let parsed =
                parse_openai_models_envelope(payload, provider, base_url, format, transport)?;
            parsed
                .into_iter()
                .map(|entry| apply_opencode_route(entry, preset.unwrap()))
                .collect()
        }
        Some(ProviderPreset::Xai | ProviderPreset::XaiOauth) => {
            let oauth = matches!(preset, Some(ProviderPreset::XaiOauth));
            parse_openai_models_envelope(payload, provider, base_url, format, transport)?
                .into_iter()
                .map(|entry| apply_xai_policy(entry, oauth))
                .collect()
        }
        _ if format == wakuwaku_provider::ApiFormat::Anthropic => {
            parse_anthropic_models_envelope(payload, provider, base_url)?
        }
        _ => parse_openai_models_envelope(payload, provider, base_url, format, transport)?,
    };
    Ok(models)
}

pub fn auth_headers(config: &ProviderConfig) -> Result<reqwest::header::HeaderMap, HarnessError> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    match &config.auth {
        Auth::Bearer(key) => {
            let value = format!("Bearer {key}");
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::try_from(value)
                    .map_err(|_| HarnessError::InvalidRequest("invalid bearer".into()))?,
            );
        }
        Auth::AnthropicApiKey { key, version } => {
            headers.insert(
                "x-api-key",
                reqwest::header::HeaderValue::try_from(key.as_str())
                    .map_err(|_| HarnessError::InvalidRequest("invalid x-api-key".into()))?,
            );
            headers.insert(
                "anthropic-version",
                reqwest::header::HeaderValue::try_from(version.as_str()).map_err(|_| {
                    HarnessError::InvalidRequest("invalid anthropic-version".into())
                })?,
            );
        }
        Auth::None => {}
    }
    if config.transport == TransportProfile::Codex {
        headers.insert(
            "openai-beta",
            reqwest::header::HeaderValue::from_static("responses=experimental"),
        );
        headers.insert(
            "originator",
            reqwest::header::HeaderValue::from_static(
                wakuwaku_provider::AuthEndpoints::CODEX_ORIGINATOR,
            ),
        );
        headers.insert(
            "version",
            reqwest::header::HeaderValue::from_static(
                wakuwaku_provider::AuthEndpoints::client_version(),
            ),
        );
        for (name, value) in &config.extra_auth_headers {
            if let (Ok(header_name), Ok(header_value)) = (
                reqwest::header::HeaderName::try_from(name.as_str()),
                reqwest::header::HeaderValue::try_from(value.as_str()),
            ) {
                headers.insert(header_name, header_value);
            }
        }
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wakuwaku_provider::{ApiFormat, ProviderId};

    #[test]
    fn models_url_joins_v1_root() {
        assert_eq!(
            models_url("https://api.openai.com/v1").unwrap(),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn codex_models_url_includes_canonical_client_version() {
        let url = models_url_for(
            Some(ProviderPreset::OpenAiCodex),
            "https://chatgpt.com/backend-api/codex",
        )
        .unwrap();
        let version = wakuwaku_provider::AuthEndpoints::client_version();
        assert!(url.contains(&format!("client_version={version}")), "{url}");
        assert!(version.starts_with("waku-"));
    }

    #[test]
    fn go_override_is_applied_during_parse() {
        let models = parse_models_payload(
            Some(ProviderPreset::OpenCodeGo),
            &json!({ "data": [{ "id": "deepseek-v4-flash" }, { "id": "mystery" }] }),
            ProviderId::new(ProviderId::OPENCODE_GO),
            "https://opencode.ai/zen/go/v1",
            ApiFormat::OpenAiChat,
            TransportProfile::Standard,
        )
        .unwrap();
        assert_eq!(models[0].api_format, ApiFormat::OpenAiResponses);
        assert!(models[0].supported);
        assert!(!models[1].supported);
    }

    #[test]
    fn custom_anthropic_uses_display_name_envelope() {
        let models = parse_models_payload(
            None,
            &json!({
                "data": [
                    { "id": "claude-sonnet-4-5", "display_name": "Sonnet" },
                    { "id": "claude-opus-4-6", "name": "ignored" }
                ]
            }),
            ProviderId::new("corp-anthropic"),
            "http://127.0.0.1:9/v1",
            ApiFormat::Anthropic,
            TransportProfile::Standard,
        )
        .unwrap();
        assert_eq!(models[0].name, "Sonnet");
        assert_eq!(models[0].api_format, ApiFormat::Anthropic);
        assert!(models[0].supported);
        assert!(models[0].capabilities.reasoning_effort);
        assert!(!models[0].capabilities.service_tier);
    }
}
