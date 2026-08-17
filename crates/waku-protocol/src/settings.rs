use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::ProviderPreset;
use crate::model::ExternalProvider;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, TS)]
#[serde(default)]
pub struct DaemonSettings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_providers: Vec<ExternalProvider>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl DaemonSettings {
    pub fn validate(&self) -> Result<(), String> {
        let mut ids = HashSet::with_capacity(self.external_providers.len());
        for provider in &self.external_providers {
            provider.validate()?;
            if ProviderPreset::parse_id(provider.id.as_str()).is_some() {
                return Err(format!(
                    "external provider id {:?} is reserved for a built-in preset; use a distinct custom id",
                    provider.id.as_str()
                ));
            }
            if !ids.insert(provider.id.clone()) {
                return Err(format!(
                    "duplicate external provider id {:?}",
                    provider.id.as_str()
                ));
            }
        }
        Ok(())
    }

    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".waku")
            .join("settings.json")
    }

    pub fn discard_legacy_app_keys(&mut self) {
        for key in [
            "analytics_enabled",
            "favorite_models",
            "theme",
            "language",
            "disabled_providers",
            "provider_binary_overrides",
            "computer_use_enabled",
            "computer_use_allowed_apps",
        ] {
            self.extra.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApiFormat, ExternalProvider, ProviderId};

    #[test]
    fn reserved_ids_cannot_be_saved_as_custom() {
        let settings = DaemonSettings {
            external_providers: vec![ExternalProvider::new(
                ProviderId::OPENAI_RESPONSES,
                "Override",
                "http://127.0.0.1:9/v1",
                ApiFormat::OpenAiResponses,
                "gpt-5",
            )],
            extra: Default::default(),
        };
        let error = settings.validate().unwrap_err();
        assert!(error.contains("reserved"), "{error}");
    }

    #[test]
    fn custom_ids_validate() {
        let settings = DaemonSettings {
            external_providers: vec![ExternalProvider::new(
                "corp-responses",
                "Corp",
                "http://127.0.0.1:9/v1",
                ApiFormat::OpenAiResponses,
                "gpt-5",
            )],
            extra: Default::default(),
        };
        settings.validate().unwrap();
    }
}
