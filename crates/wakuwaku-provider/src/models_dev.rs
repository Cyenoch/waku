//! Models.dev metadata used to fill missing catalog effort and Fast flags.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::catalog::normalize_reasoning_effort;
use crate::{ApiFormat, ModelCatalogEntry, ProviderPreset, ReasoningEffortOption};

pub const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";

/// Lookup table of Models.dev provider → model metadata.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelsDevCatalog {
    providers: BTreeMap<String, BTreeMap<String, ModelsDevModel>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ModelsDevModel {
    effort_values: Vec<String>,
    priority_service_tier: bool,
}

impl ModelsDevCatalog {
    fn get(&self, source: &str, model_id: &str) -> Option<&ModelsDevModel> {
        self.providers.get(source)?.get(model_id)
    }
}

/// Exact Models.dev provider key for a first-party preset. Never aliases Codex
/// or xAI OAuth/SuperGrok onto another product.
pub fn models_dev_source_key(preset: ProviderPreset) -> Option<&'static str> {
    match preset {
        ProviderPreset::OpenAiResponses | ProviderPreset::OpenAiChat => Some("openai"),
        ProviderPreset::Anthropic => Some("anthropic"),
        ProviderPreset::OpenCodeZen => Some("opencode"),
        ProviderPreset::OpenCodeGo => Some("opencode-go"),
        ProviderPreset::Xai => Some("xai"),
        ProviderPreset::OpenAiCodex | ProviderPreset::XaiOauth => None,
    }
}

pub fn parse_models_dev_document(payload: &Value) -> ModelsDevCatalog {
    let Some(root) = payload.as_object() else {
        return ModelsDevCatalog::default();
    };
    let mut providers = BTreeMap::new();
    for (provider_id, provider) in root {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        let mut parsed = BTreeMap::new();
        for (model_id, model) in models {
            if model_id.is_empty() || !model.is_object() {
                continue;
            }
            parsed.insert(model_id.clone(), parse_model(model));
        }
        if !parsed.is_empty() {
            providers.insert(provider_id.clone(), parsed);
        }
    }
    ModelsDevCatalog { providers }
}

fn parse_model(model: &Value) -> ModelsDevModel {
    ModelsDevModel {
        effort_values: effort_values(model),
        priority_service_tier: has_priority_service_tier(model),
    }
}

fn effort_values(model: &Value) -> Vec<String> {
    let Some(options) = model.get("reasoning_options").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for option in options {
        if option.get("type").and_then(Value::as_str) != Some("effort") {
            continue;
        }
        let Some(raw_values) = option.get("values").and_then(Value::as_array) else {
            continue;
        };
        for value in raw_values {
            let Some(text) = value
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
            else {
                continue;
            };
            if values.iter().any(|existing| existing == text) {
                continue;
            }
            values.push(text.to_owned());
        }
    }
    values
}

fn has_priority_service_tier(model: &Value) -> bool {
    model
        .pointer("/experimental/modes/fast/provider/body/service_tier")
        .and_then(Value::as_str)
        == Some("priority")
}

/// Fill missing effort options and exact Priority Fast from Models.dev.
///
/// Live nonempty efforts, labels, and default stay untouched. Unsupported
/// rows and presets without a source key are left as-is.
pub fn enrich_catalog_from_models_dev(
    mut entries: Vec<ModelCatalogEntry>,
    preset: ProviderPreset,
    source: &ModelsDevCatalog,
) -> Vec<ModelCatalogEntry> {
    let Some(source_key) = models_dev_source_key(preset) else {
        return entries;
    };
    for entry in &mut entries {
        if !entry.supported {
            continue;
        }
        let Some(meta) = source.get(source_key, &entry.id) else {
            continue;
        };
        if entry.reasoning_efforts.is_empty() {
            apply_source_efforts(entry, &meta.effort_values);
        }
        if meta.priority_service_tier && can_forward_priority(entry) {
            entry.capabilities.service_tier = true;
        }
    }
    entries
}

fn apply_source_efforts(entry: &mut ModelCatalogEntry, values: &[String]) {
    let mut efforts = Vec::with_capacity(values.len());
    for provider_value in values {
        let id = normalize_reasoning_effort(provider_value, entry);
        if efforts
            .iter()
            .any(|effort: &ReasoningEffortOption| effort.id == id)
        {
            continue;
        }
        efforts.push(ReasoningEffortOption {
            id,
            provider_value: provider_value.clone(),
            label: provider_value.clone(),
        });
    }
    if efforts.is_empty() {
        return;
    }
    entry.reasoning_efforts = efforts;
    entry.reasoning = true;
    entry.capabilities.reasoning_effort = true;
}

fn can_forward_priority(entry: &ModelCatalogEntry) -> bool {
    matches!(
        entry.api_format,
        ApiFormat::OpenAiResponses | ApiFormat::OpenAiChat
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ModelCapabilities, ProviderId, TransportProfile, apply_opencode_route, apply_xai_policy,
        parse_openai_models_envelope,
    };
    use serde_json::json;

    fn live_openai(id: &str) -> ModelCatalogEntry {
        parse_openai_models_envelope(
            &json!({ "data": [{ "id": id }] }),
            ProviderId::new(ProviderId::OPENAI_RESPONSES),
            "https://api.openai.com/v1",
            ApiFormat::OpenAiResponses,
            TransportProfile::Standard,
        )
        .unwrap()
        .remove(0)
    }

    fn fixture() -> ModelsDevCatalog {
        parse_models_dev_document(&json!({
            "openai": {
                "models": {
                    "gpt-5.5": {
                        "reasoning_options": [{
                            "type": "effort",
                            "values": ["none", "low", null, "medium", "high", "default"]
                        }],
                        "experimental": {
                            "modes": {
                                "fast": {
                                    "provider": { "body": { "service_tier": "priority" } }
                                }
                            }
                        }
                    },
                    "gpt-5-nano": {
                        "reasoning_options": [{
                            "type": "effort",
                            "values": ["minimal", "low", "medium", "high"]
                        }]
                    },
                    "gpt-5.4": {
                        "reasoning_options": [{ "type": "toggle" }],
                        "experimental": {
                            "modes": {
                                "fast": {
                                    "provider": { "body": { "service_tier": "flex" } }
                                }
                            }
                        }
                    }
                }
            },
            "anthropic": {
                "models": {
                    "claude-opus-4-8": {
                        "reasoning_options": [
                            { "type": "effort", "values": ["low", "medium", "high"] },
                            { "type": "budget_tokens", "min": 1024 }
                        ],
                        "experimental": {
                            "modes": {
                                "fast": {
                                    "provider": {
                                        "body": { "speed": "fast" },
                                        "headers": { "anthropic-internal-smart-steering": "true" }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "xai": {
                "models": {
                    "grok-4.5": {
                        "reasoning_options": [{
                            "type": "effort",
                            "values": ["low", "medium", "high"]
                        }]
                    }
                }
            },
            "opencode": {
                "models": {
                    "gpt-5.5": {
                        "reasoning_options": [{
                            "type": "effort",
                            "values": ["low", "high"]
                        }]
                    }
                }
            }
        }))
    }

    #[test]
    fn source_keys_never_alias_codex_or_xai_oauth() {
        assert_eq!(
            models_dev_source_key(ProviderPreset::OpenAiResponses),
            Some("openai")
        );
        assert_eq!(
            models_dev_source_key(ProviderPreset::OpenAiChat),
            Some("openai")
        );
        assert_eq!(
            models_dev_source_key(ProviderPreset::Anthropic),
            Some("anthropic")
        );
        assert_eq!(models_dev_source_key(ProviderPreset::Xai), Some("xai"));
        assert_eq!(
            models_dev_source_key(ProviderPreset::OpenCodeZen),
            Some("opencode")
        );
        assert_eq!(
            models_dev_source_key(ProviderPreset::OpenCodeGo),
            Some("opencode-go")
        );
        assert_eq!(models_dev_source_key(ProviderPreset::OpenAiCodex), None);
        assert_eq!(models_dev_source_key(ProviderPreset::XaiOauth), None);
    }

    #[test]
    fn official_openai_live_parse_clears_baseline_fast() {
        let entry = live_openai("gpt-5.5");
        assert!(!entry.capabilities.service_tier);
        assert!(entry.reasoning_efforts.is_empty());
        assert!(entry.default_reasoning_effort.is_none());
    }

    #[test]
    fn openai_exact_priority_enables_fast_and_keeps_raw_efforts() {
        let enriched = enrich_catalog_from_models_dev(
            vec![live_openai("gpt-5.5")],
            ProviderPreset::OpenAiResponses,
            &fixture(),
        );
        assert!(enriched[0].capabilities.service_tier);
        assert_eq!(
            enriched[0]
                .reasoning_efforts
                .iter()
                .map(|effort| {
                    (
                        effort.id.as_str(),
                        effort.provider_value.as_str(),
                        effort.label.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                ("none", "none", "none"),
                ("low", "low", "low"),
                ("medium", "medium", "medium"),
                ("high", "high", "high"),
                (
                    "custom:openai-responses:gpt-5.5:default",
                    "default",
                    "default"
                ),
            ]
        );
        assert!(enriched[0].default_reasoning_effort.is_none());
    }

    #[test]
    fn openai_model_without_priority_stays_fast_false() {
        let enriched = enrich_catalog_from_models_dev(
            vec![live_openai("gpt-5-nano"), live_openai("gpt-5.4")],
            ProviderPreset::OpenAiChat,
            &fixture(),
        );
        assert!(!enriched[0].capabilities.service_tier);
        assert!(!enriched[1].capabilities.service_tier);
        assert_eq!(
            enriched[0]
                .reasoning_efforts
                .iter()
                .map(|effort| effort.provider_value.as_str())
                .collect::<Vec<_>>(),
            ["minimal", "low", "medium", "high"]
        );
        assert!(enriched[1].reasoning_efforts.is_empty());
    }

    #[test]
    fn live_nonempty_efforts_and_default_win() {
        let mut entry = live_openai("gpt-5.5");
        entry.reasoning_efforts = vec![ReasoningEffortOption {
            id: "high".into(),
            provider_value: "deep".into(),
            label: "Deep Thought".into(),
        }];
        entry.default_reasoning_effort = Some("high".into());
        let enriched = enrich_catalog_from_models_dev(
            vec![entry],
            ProviderPreset::OpenAiResponses,
            &fixture(),
        );
        assert_eq!(enriched[0].reasoning_efforts[0].provider_value, "deep");
        assert_eq!(enriched[0].reasoning_efforts[0].label, "Deep Thought");
        assert_eq!(
            enriched[0].default_reasoning_effort.as_deref(),
            Some("high")
        );
        assert!(enriched[0].capabilities.service_tier);
    }

    #[test]
    fn opencode_does_not_inherit_openai_priority() {
        let mut entry = apply_opencode_route(live_openai("gpt-5.5"), ProviderPreset::OpenCodeZen);
        entry.provider = ProviderId::new(ProviderId::OPENCODE_ZEN);
        let enriched =
            enrich_catalog_from_models_dev(vec![entry], ProviderPreset::OpenCodeZen, &fixture());
        assert!(!enriched[0].capabilities.service_tier);
        assert_eq!(
            enriched[0]
                .reasoning_efforts
                .iter()
                .map(|effort| effort.provider_value.as_str())
                .collect::<Vec<_>>(),
            ["low", "high"]
        );
    }

    #[test]
    fn anthropic_speed_header_fast_does_not_enable_service_tier() {
        let mut entry = live_openai("claude-opus-4-8");
        entry.provider = ProviderId::new(ProviderId::ANTHROPIC);
        entry.api_format = ApiFormat::Anthropic;
        entry.capabilities = ModelCapabilities::anthropic();
        let enriched =
            enrich_catalog_from_models_dev(vec![entry], ProviderPreset::Anthropic, &fixture());
        assert!(!enriched[0].capabilities.service_tier);
        assert_eq!(
            enriched[0]
                .reasoning_efforts
                .iter()
                .map(|effort| effort.provider_value.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high"]
        );
    }

    #[test]
    fn xai_api_exact_model_gets_efforts_without_fast() {
        let entry = apply_xai_policy(
            parse_openai_models_envelope(
                &json!({ "data": [{ "id": "grok-4.5" }] }),
                ProviderId::new(ProviderId::XAI),
                "https://api.x.ai/v1",
                ApiFormat::OpenAiResponses,
                TransportProfile::Standard,
            )
            .unwrap()
            .remove(0),
            false,
        );
        let enriched = enrich_catalog_from_models_dev(vec![entry], ProviderPreset::Xai, &fixture());
        assert!(!enriched[0].capabilities.service_tier);
        assert_eq!(
            enriched[0]
                .reasoning_efforts
                .iter()
                .map(|effort| (effort.id.as_str(), effort.provider_value.as_str()))
                .collect::<Vec<_>>(),
            [("low", "low"), ("medium", "medium"), ("high", "high")]
        );
    }

    #[test]
    fn xai_oauth_is_not_aliased_to_xai() {
        let entry = apply_xai_policy(
            parse_openai_models_envelope(
                &json!({ "data": [{ "id": "grok-4.5" }] }),
                ProviderId::new(ProviderId::XAI_OAUTH),
                "https://api.x.ai/v1",
                ApiFormat::OpenAiResponses,
                TransportProfile::Standard,
            )
            .unwrap()
            .remove(0),
            true,
        );
        let enriched =
            enrich_catalog_from_models_dev(vec![entry], ProviderPreset::XaiOauth, &fixture());
        assert!(enriched[0].reasoning_efforts.is_empty());
        assert!(!enriched[0].capabilities.service_tier);
    }

    #[test]
    fn unknown_model_and_malformed_source_leave_options_empty() {
        let missing = enrich_catalog_from_models_dev(
            vec![live_openai("mystery-sku")],
            ProviderPreset::OpenAiResponses,
            &fixture(),
        );
        assert!(missing[0].reasoning_efforts.is_empty());
        assert!(!missing[0].capabilities.service_tier);

        let malformed = enrich_catalog_from_models_dev(
            vec![live_openai("gpt-5.5")],
            ProviderPreset::OpenAiResponses,
            &parse_models_dev_document(&json!(["not", "an", "object"])),
        );
        assert!(malformed[0].reasoning_efforts.is_empty());
        assert!(!malformed[0].capabilities.service_tier);
    }

    #[test]
    fn unsupported_rows_stay_unsupported_and_unenriched() {
        let mut entry = live_openai("gpt-5.5");
        entry.supported = false;
        let enriched = enrich_catalog_from_models_dev(
            vec![entry],
            ProviderPreset::OpenAiResponses,
            &fixture(),
        );
        assert!(!enriched[0].supported);
        assert!(enriched[0].reasoning_efforts.is_empty());
        assert!(!enriched[0].capabilities.service_tier);
    }
}
