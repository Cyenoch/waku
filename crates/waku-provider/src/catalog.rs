//! Model catalog entries, grounded list parsers, and OpenCode routing.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::{
    ApiFormat, ModelCapabilities, ProviderId, ProviderLimits, ProviderPreset, TransportProfile,
    UnsupportedReason,
};

/// One discovered or seeded model that a session can select.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogEntry {
    pub id: String,
    pub name: String,
    pub provider: ProviderId,
    pub api_format: ApiFormat,
    pub transport: TransportProfile,
    pub base_url: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub reasoning: bool,
    pub capabilities: ModelCapabilities,
    pub supported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<UnsupportedReason>,
}

impl ModelCatalogEntry {
    pub fn limits(&self) -> ProviderLimits {
        ProviderLimits {
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("model catalog response was not a recognized envelope")]
    UnrecognizedEnvelope,
    #[error("model catalog response contained no usable models")]
    Empty,
}

/// Grounded OpenAI-compatible envelope: `{ "data": [ { "id": "..." } ] }`.
pub fn parse_openai_models_envelope(
    payload: &Value,
    provider: ProviderId,
    base_url: &str,
    default_format: ApiFormat,
    transport: TransportProfile,
) -> Result<Vec<ModelCatalogEntry>, CatalogError> {
    let rows = object_array_field(payload, "data").ok_or(CatalogError::UnrecognizedEnvelope)?;
    let mut models = Vec::new();
    for row in rows {
        let Some(id) = string_field(row, "id") else {
            continue;
        };
        let name = string_field(row, "name").unwrap_or(id);
        let mut entry = standard_entry(
            id,
            name,
            provider.clone(),
            base_url,
            default_format,
            transport,
        );
        if is_openai_non_chat_model(id) {
            entry.supported = false;
            entry.unsupported_reason = Some(UnsupportedReason::NonChat);
        }
        models.push(entry);
    }
    Ok(models)
}

/// OpenAI `/models` ids that are not Chat Completions or Responses.
///
/// The Models API does not label chat-vs-embedding. Only documented
/// non-conversation families are marked unsupported. Chat-vs-Responses
/// is not inferred from the id.
pub fn is_openai_non_chat_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "text-embedding-",
        "text-similarity-",
        "text-search-",
        "text-moderation-",
        "omni-moderation-",
        "whisper-",
        "tts-",
        "dall-e-",
        "gpt-image-",
        "chatgpt-image-",
        "sora-",
        "babbage-",
        "davinci-",
        "text-davinci-",
        "text-curie-",
        "text-ada-",
        "text-babbage-",
    ];
    PREFIXES.iter().any(|prefix| id.starts_with(prefix))
        || id.contains("-embedding")
        || id.contains("moderation")
        || id.ends_with("-transcribe")
        || id.contains("-transcribe-")
        || id.ends_with("-tts")
}

/// Grounded Anthropic envelope: `{ "data": [ { "id": "...", "display_name": "..." } ] }`.
pub fn parse_anthropic_models_envelope(
    payload: &Value,
    provider: ProviderId,
    base_url: &str,
) -> Result<Vec<ModelCatalogEntry>, CatalogError> {
    let rows = object_array_field(payload, "data").ok_or(CatalogError::UnrecognizedEnvelope)?;
    let mut models = Vec::new();
    for row in rows {
        let Some(id) = string_field(row, "id") else {
            continue;
        };
        let name = string_field(row, "display_name").unwrap_or(id);
        let mut entry = standard_entry(
            id,
            name,
            provider.clone(),
            base_url,
            ApiFormat::Anthropic,
            TransportProfile::Standard,
        );
        entry.capabilities = ModelCapabilities::anthropic();
        models.push(entry);
    }
    Ok(models)
}

/// Grounded Codex envelope: `{ "models"|"data": [ { "slug"|"id", ... } ] }`.
pub fn parse_codex_models_envelope(
    payload: &Value,
    provider: ProviderId,
    base_url: &str,
) -> Result<Vec<ModelCatalogEntry>, CatalogError> {
    let rows = object_array_field(payload, "models")
        .or_else(|| object_array_field(payload, "data"))
        .ok_or(CatalogError::UnrecognizedEnvelope)?;
    let mut models = Vec::new();
    for row in rows {
        let Some(id) = string_field(row, "slug").or_else(|| string_field(row, "id")) else {
            continue;
        };
        if matches!(
            string_field(row, "visibility")
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("hide" | "hidden")
        ) {
            continue;
        }
        let name = string_field(row, "display_name").unwrap_or(id);
        let context_window =
            positive_u64(row, "context_window").unwrap_or_else(|| default_codex_context(id));
        let max_output_tokens = context_window.min(128_000);
        let reasoning = row.get("default_reasoning_level").is_some()
            || row
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .is_some_and(|levels| !levels.is_empty());
        models.push(ModelCatalogEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            provider: provider.clone(),
            api_format: ApiFormat::OpenAiResponses,
            transport: TransportProfile::Codex,
            base_url: base_url.to_owned(),
            context_window,
            max_output_tokens,
            reasoning,
            capabilities: ModelCapabilities::codex(),
            supported: true,
            unsupported_reason: None,
        });
    }
    Ok(models)
}

fn default_codex_context(id: &str) -> u64 {
    let lower = id.to_ascii_lowercase();
    if lower.contains("gpt-5.6")
        && (lower.contains("luna") || lower.contains("sol") || lower.contains("terra"))
    {
        372_000
    } else {
        272_000
    }
}

fn capabilities_for(provider: &ProviderId, format: ApiFormat) -> ModelCapabilities {
    match provider.as_str() {
        id if id == ProviderId::OPENAI_RESPONSES || id == ProviderId::OPENAI_CHAT => {
            ModelCapabilities::openai_api(format)
        }
        id if id == ProviderId::OPENAI_CODEX => ModelCapabilities::codex(),
        _ => ModelCapabilities::openai_compatible(format),
    }
}

fn standard_entry(
    id: &str,
    name: &str,
    provider: ProviderId,
    base_url: &str,
    format: ApiFormat,
    transport: TransportProfile,
) -> ModelCatalogEntry {
    let limits = ProviderLimits::default();
    let capabilities = capabilities_for(&provider, format);
    ModelCatalogEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        provider,
        api_format: format,
        transport,
        base_url: base_url.to_owned(),
        context_window: limits.context_window,
        max_output_tokens: limits.max_output_tokens,
        reasoning: false,
        capabilities,
        supported: true,
        unsupported_reason: None,
    }
}

fn object_array_field<'a>(payload: &'a Value, field: &str) -> Option<&'a Vec<Value>> {
    payload
        .as_object()?
        .get(field)?
        .as_array()
        .filter(|rows| rows.iter().all(|row| row.is_object() || row.is_null()))
}

fn string_field<'a>(row: &'a Value, field: &str) -> Option<&'a str> {
    row.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn positive_u64(row: &Value, field: &str) -> Option<u64> {
    let value = row.get(field)?;
    let number = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| {
            value
                .as_f64()
                .and_then(|n| (n.is_finite() && n > 0.0).then_some(n as u64))
        })?;
    (number > 0).then_some(number)
}

/// Decision for one OpenCode Go/Zen model id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenCodeRoute {
    Format(ApiFormat),
    Unsupported(UnsupportedReason),
}

/// Exhaustive typed routing for known OpenCode families plus named overrides.
pub fn route_opencode_model(preset: ProviderPreset, model_id: &str) -> OpenCodeRoute {
    let id = model_id.trim().to_ascii_lowercase();
    match (preset, id.as_str()) {
        (ProviderPreset::OpenCodeZen, "minimax-m3" | "minimax-m3-free") => {
            return OpenCodeRoute::Format(ApiFormat::OpenAiChat);
        }
        (ProviderPreset::OpenCodeGo, "deepseek-v4-flash") => {
            return OpenCodeRoute::Format(ApiFormat::OpenAiResponses);
        }
        (
            ProviderPreset::OpenCodeGo,
            "minimax-m2.7" | "minimax-m3" | "minimax-m3-free" | "qwen3.5-plus" | "qwen3.6-plus",
        ) => return OpenCodeRoute::Format(ApiFormat::OpenAiChat),
        _ => {}
    }
    if id.starts_with("gemini-") {
        return OpenCodeRoute::Unsupported(UnsupportedReason::GoogleFormat);
    }
    if id.starts_with("gpt-") || id.starts_with("grok-") || id.starts_with("muse-") {
        return OpenCodeRoute::Format(ApiFormat::OpenAiResponses);
    }
    if id.starts_with("claude-") || id.starts_with("qwen") {
        return OpenCodeRoute::Format(ApiFormat::Anthropic);
    }
    if id.starts_with("deepseek-")
        || id.starts_with("glm-")
        || id.starts_with("kimi-")
        || id.starts_with("minimax-")
        || id.starts_with("mimo-")
        || id.starts_with("hy3")
        || id == "big-pickle"
        || id.ends_with("-free")
    {
        return OpenCodeRoute::Format(ApiFormat::OpenAiChat);
    }
    OpenCodeRoute::Unsupported(UnsupportedReason::Unroutable)
}

pub fn apply_opencode_route(
    mut entry: ModelCatalogEntry,
    preset: ProviderPreset,
) -> ModelCatalogEntry {
    match route_opencode_model(preset, &entry.id) {
        OpenCodeRoute::Format(format) => {
            entry.api_format = format;
            entry.transport = TransportProfile::Standard;
            entry.supported = true;
            entry.unsupported_reason = None;
            entry.capabilities = match format {
                ApiFormat::Anthropic => ModelCapabilities::anthropic(),
                other => ModelCapabilities::openai_compatible(other),
            };
        }
        OpenCodeRoute::Unsupported(reason) => {
            entry.supported = false;
            entry.unsupported_reason = Some(reason);
        }
    }
    entry
}

const XAI_NON_CHAT_PREFIXES: [&str; 3] = ["grok-imagine-", "grok-stt-", "grok-voice-"];

pub fn is_xai_non_chat_model(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    XAI_NON_CHAT_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

pub fn is_grok_reasoning_effort_capable(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "grok-4" | "grok-4.3" | "grok-4.5" | "grok-4.6" | "grok-3" | "grok-3-mini"
    ) || (lower.starts_with("grok-4.")
        && !lower.contains("build")
        && !lower.contains("code-fast")
        && !lower.contains("composer")
        && !lower.contains("4.20"))
}

pub fn apply_xai_policy(mut entry: ModelCatalogEntry, oauth: bool) -> ModelCatalogEntry {
    if is_xai_non_chat_model(&entry.id) {
        entry.supported = false;
        entry.unsupported_reason = Some(UnsupportedReason::NonChat);
        return entry;
    }
    let effort = is_grok_reasoning_effort_capable(&entry.id);
    entry.api_format = ApiFormat::OpenAiResponses;
    entry.transport = TransportProfile::Standard;
    entry.capabilities = ModelCapabilities::xai(effort);
    entry.reasoning = effort || entry.reasoning;
    if oauth {
        entry.max_output_tokens = entry.context_window.clamp(1, 64_000);
    }
    entry
}

pub fn xai_oauth_seed(base_url: &str) -> Vec<ModelCatalogEntry> {
    const SEED: &[(&str, &str, u64, bool, bool)] = &[
        ("grok-build", "Grok Build", 512_000, true, false),
        ("grok-build-0.1", "Grok Build 0.1", 256_000, true, false),
        ("grok-4.3", "Grok 4.3", 1_000_000, true, true),
        ("grok-4.5", "Grok 4.5", 500_000, true, true),
        ("grok-4.6", "Grok 4.6", 500_000, true, true),
        (
            "grok-4.20-multi-agent-0309",
            "Grok 4.20 (Multi-Agent)",
            2_000_000,
            true,
            false,
        ),
        (
            "grok-4.20-0309-reasoning",
            "Grok 4.20 (Reasoning)",
            2_000_000,
            true,
            false,
        ),
        (
            "grok-4.20-0309-non-reasoning",
            "Grok 4.20 (Non-Reasoning)",
            2_000_000,
            false,
            false,
        ),
        (
            "grok-composer-2.5-fast",
            "Grok Composer 2.5 Fast",
            200_000,
            false,
            false,
        ),
    ];
    SEED.iter()
        .map(|(id, name, context, reasoning, effort)| ModelCatalogEntry {
            id: (*id).to_owned(),
            name: (*name).to_owned(),
            provider: ProviderId::new(ProviderId::XAI_OAUTH),
            api_format: ApiFormat::OpenAiResponses,
            transport: TransportProfile::Standard,
            base_url: base_url.to_owned(),
            context_window: *context,
            max_output_tokens: (*context).min(64_000),
            reasoning: *reasoning,
            capabilities: ModelCapabilities::xai(*effort),
            supported: true,
            unsupported_reason: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_parser_requires_data_array_of_objects() {
        let err = parse_openai_models_envelope(
            &json!({ "models": [{ "id": "x" }] }),
            ProviderId::new("openai-responses"),
            "https://api.openai.com/v1",
            ApiFormat::OpenAiResponses,
            TransportProfile::Standard,
        )
        .unwrap_err();
        assert_eq!(err, CatalogError::UnrecognizedEnvelope);
        let models = parse_openai_models_envelope(
            &json!({ "data": [{ "id": "gpt-5", "name": "GPT 5" }] }),
            ProviderId::new("openai-responses"),
            "https://api.openai.com/v1",
            ApiFormat::OpenAiResponses,
            TransportProfile::Standard,
        )
        .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5");
    }

    #[test]
    fn openai_parser_rejects_broad_json() {
        let err = parse_openai_models_envelope(
            &json!({ "result": { "nested": [{ "id": "nope" }] } }),
            ProviderId::new("xai"),
            "https://api.x.ai/v1",
            ApiFormat::OpenAiResponses,
            TransportProfile::Standard,
        )
        .unwrap_err();
        assert_eq!(err, CatalogError::UnrecognizedEnvelope);
    }

    #[test]
    fn codex_parser_drops_hidden_and_falls_back_context() {
        let models = parse_codex_models_envelope(
            &json!({
                "models": [
                    { "slug": "gpt-5.4", "display_name": "GPT-5.4", "visibility": "hide" },
                    { "id": "gpt-5.6-luna", "display_name": "Luna" }
                ]
            }),
            ProviderId::new(ProviderId::OPENAI_CODEX),
            "https://chatgpt.com/backend-api/codex",
        )
        .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5.6-luna");
        assert_eq!(models[0].context_window, 372_000);
        assert_eq!(models[0].transport, TransportProfile::Codex);
        assert_eq!(models[0].api_format, ApiFormat::OpenAiResponses);
        assert!(!models[0].capabilities.sampling);
        assert!(!models[0].capabilities.service_tier);
    }

    #[test]
    fn opencode_named_overrides_and_unknown_are_explicit() {
        assert_eq!(
            route_opencode_model(ProviderPreset::OpenCodeZen, "minimax-m3"),
            OpenCodeRoute::Format(ApiFormat::OpenAiChat)
        );
        assert_eq!(
            route_opencode_model(ProviderPreset::OpenCodeGo, "deepseek-v4-flash"),
            OpenCodeRoute::Format(ApiFormat::OpenAiResponses)
        );
        assert_eq!(
            route_opencode_model(ProviderPreset::OpenCodeGo, "qwen3.6-plus"),
            OpenCodeRoute::Format(ApiFormat::OpenAiChat)
        );
        assert_eq!(
            route_opencode_model(ProviderPreset::OpenCodeZen, "gemini-3.5-flash"),
            OpenCodeRoute::Unsupported(UnsupportedReason::GoogleFormat)
        );
        assert_eq!(
            route_opencode_model(ProviderPreset::OpenCodeGo, "totally-unknown-sku"),
            OpenCodeRoute::Unsupported(UnsupportedReason::Unroutable)
        );
        assert_eq!(
            route_opencode_model(ProviderPreset::OpenCodeZen, "claude-opus-4-8"),
            OpenCodeRoute::Format(ApiFormat::Anthropic)
        );
    }

    #[test]
    fn xai_filters_non_chat_and_effort_allowlist() {
        assert!(is_xai_non_chat_model("grok-imagine-image"));
        assert!(!is_xai_non_chat_model("grok-4.5"));
        assert!(is_grok_reasoning_effort_capable("grok-4.5"));
        assert!(!is_grok_reasoning_effort_capable("grok-build"));
        assert!(!is_grok_reasoning_effort_capable(
            "grok-4.20-0309-reasoning"
        ));
        let mut entry = standard_entry(
            "grok-imagine-1",
            "Imagine",
            ProviderId::new(ProviderId::XAI),
            "https://api.x.ai/v1",
            ApiFormat::OpenAiResponses,
            TransportProfile::Standard,
        );
        entry = apply_xai_policy(entry, false);
        assert!(!entry.supported);
        assert_eq!(entry.unsupported_reason, Some(UnsupportedReason::NonChat));
    }

    #[test]
    fn openai_marks_documented_non_chat_and_keeps_conversation_ids() {
        let models = parse_openai_models_envelope(
            &json!({
                "data": [
                    { "id": "gpt-5" },
                    { "id": "gpt-4o-audio-preview" },
                    { "id": "o3" },
                    { "id": "text-embedding-3-small" },
                    { "id": "whisper-1" },
                    { "id": "tts-1-hd" },
                    { "id": "dall-e-3" },
                    { "id": "gpt-image-1" },
                    { "id": "omni-moderation-latest" },
                    { "id": "gpt-4o-transcribe" },
                    { "id": "gpt-4o-mini-tts" },
                    { "id": "sora-2" },
                    { "id": "babbage-002" }
                ]
            }),
            ProviderId::new(ProviderId::OPENAI_RESPONSES),
            "https://api.openai.com/v1",
            ApiFormat::OpenAiResponses,
            TransportProfile::Standard,
        )
        .unwrap();
        let by_id: std::collections::BTreeMap<_, _> = models
            .into_iter()
            .map(|entry| (entry.id.clone(), entry))
            .collect();
        for id in ["gpt-5", "gpt-4o-audio-preview", "o3"] {
            assert!(by_id[id].supported, "{id}");
            assert!(by_id[id].unsupported_reason.is_none(), "{id}");
        }
        for id in [
            "text-embedding-3-small",
            "whisper-1",
            "tts-1-hd",
            "dall-e-3",
            "gpt-image-1",
            "omni-moderation-latest",
            "gpt-4o-transcribe",
            "gpt-4o-mini-tts",
            "sora-2",
            "babbage-002",
        ] {
            assert!(!by_id[id].supported, "{id}");
            assert_eq!(
                by_id[id].unsupported_reason,
                Some(UnsupportedReason::NonChat),
                "{id}"
            );
        }
        assert!(!is_openai_non_chat_model("gpt-4o-audio-preview"));
        assert!(!is_openai_non_chat_model("gpt-4o-realtime-preview"));
    }

    #[test]
    fn xai_oauth_seed_lists_curated_ids_first() {
        let seed = xai_oauth_seed("https://api.x.ai/v1");
        assert_eq!(seed[0].id, "grok-build");
        assert!(seed.iter().any(|model| model.id == "grok-4.5"));
        assert!(
            seed.iter()
                .all(|model| model.provider.as_str() == "xai-oauth")
        );
    }
}
