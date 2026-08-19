//! Closed set of first-party provider presets.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{ApiFormat, ExternalProvider, ProviderId, TransportProfile};

/// Built-in provider identity. Distinct from a user-authored custom endpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ProviderPreset {
    OpenAiResponses,
    OpenAiChat,
    Anthropic,
    OpenAiCodex,
    OpenCodeZen,
    OpenCodeGo,
    Xai,
    XaiOauth,
}

/// How a preset authenticates. Never invents OAuth for API-key products.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum PresetAuthKind {
    ApiKey,
    OauthBrowser,
    OauthDevice,
}

impl ProviderPreset {
    pub const ALL: [Self; 8] = [
        Self::OpenAiResponses,
        Self::OpenAiChat,
        Self::Anthropic,
        Self::OpenAiCodex,
        Self::OpenCodeZen,
        Self::OpenCodeGo,
        Self::Xai,
        Self::XaiOauth,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenAiResponses => ProviderId::OPENAI_RESPONSES,
            Self::OpenAiChat => ProviderId::OPENAI_CHAT,
            Self::Anthropic => ProviderId::ANTHROPIC,
            Self::OpenAiCodex => ProviderId::OPENAI_CODEX,
            Self::OpenCodeZen => ProviderId::OPENCODE_ZEN,
            Self::OpenCodeGo => ProviderId::OPENCODE_GO,
            Self::Xai => ProviderId::XAI,
            Self::XaiOauth => ProviderId::XAI_OAUTH,
        }
    }

    pub fn provider_id(self) -> ProviderId {
        ProviderId::new(self.id())
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "OpenAI Responses",
            Self::OpenAiChat => "OpenAI Chat",
            Self::Anthropic => "Anthropic",
            Self::OpenAiCodex => "ChatGPT Codex",
            Self::OpenCodeZen => "OpenCode Zen",
            Self::OpenCodeGo => "OpenCode Go",
            Self::Xai => "xAI API",
            Self::XaiOauth => "SuperGrok / X Premium+",
        }
    }

    pub const fn env_key(self) -> Option<&'static str> {
        match self {
            Self::OpenAiResponses | Self::OpenAiChat => Some("OPENAI_API_KEY"),
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::OpenAiCodex => None,
            Self::OpenCodeZen | Self::OpenCodeGo => Some("OPENCODE_API_KEY"),
            Self::Xai => Some("XAI_API_KEY"),
            Self::XaiOauth => Some("XAI_OAUTH_TOKEN"),
        }
    }

    pub const fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAiResponses | Self::OpenAiChat => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::OpenAiCodex => "https://chatgpt.com/backend-api/codex",
            Self::OpenCodeZen => "https://opencode.ai/zen/v1",
            Self::OpenCodeGo => "https://opencode.ai/zen/go/v1",
            Self::Xai | Self::XaiOauth => "https://api.x.ai/v1",
        }
    }

    pub const fn default_format(self) -> ApiFormat {
        match self {
            Self::OpenAiChat => ApiFormat::OpenAiChat,
            Self::Anthropic => ApiFormat::Anthropic,
            Self::OpenAiResponses
            | Self::OpenAiCodex
            | Self::OpenCodeZen
            | Self::OpenCodeGo
            | Self::Xai
            | Self::XaiOauth => ApiFormat::OpenAiResponses,
        }
    }

    pub const fn transport(self) -> TransportProfile {
        match self {
            Self::OpenAiCodex => TransportProfile::Codex,
            _ => TransportProfile::Standard,
        }
    }

    pub const fn auth_kind(self) -> PresetAuthKind {
        match self {
            Self::OpenAiCodex => PresetAuthKind::OauthBrowser,
            Self::XaiOauth => PresetAuthKind::OauthDevice,
            _ => PresetAuthKind::ApiKey,
        }
    }

    pub const fn default_model(self) -> &'static str {
        match self {
            Self::OpenAiResponses | Self::OpenAiChat | Self::OpenAiCodex => "gpt-5",
            Self::Anthropic => "claude-sonnet-4-5",
            Self::OpenCodeZen => "claude-opus-4-8",
            Self::OpenCodeGo => "kimi-k2.7-code",
            Self::Xai | Self::XaiOauth => "grok-4.5",
        }
    }

    pub const fn models_path(self) -> &'static str {
        match self {
            Self::OpenAiCodex => "/models",
            Self::Anthropic => "/models",
            _ => "/models",
        }
    }

    pub fn parse_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.id() == id)
    }

    pub fn endpoint(self) -> ExternalProvider {
        ExternalProvider::new(
            self.id(),
            self.display_name(),
            self.default_base_url(),
            self.default_format(),
        )
    }
}

impl std::fmt::Display for ProviderPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_ids_are_stable_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for preset in ProviderPreset::ALL {
            assert!(preset.provider_id().is_valid());
            assert!(seen.insert(preset.id()));
        }
        assert_eq!(ProviderPreset::Xai.id(), "xai");
        assert_eq!(ProviderPreset::XaiOauth.id(), "xai-oauth");
        assert_eq!(ProviderPreset::OpenCodeZen.id(), "opencode-zen");
        assert_eq!(ProviderPreset::OpenCodeGo.id(), "opencode-go");
    }

    #[test]
    fn go_and_zen_share_env_name_but_not_identity() {
        assert_eq!(
            ProviderPreset::OpenCodeZen.env_key(),
            ProviderPreset::OpenCodeGo.env_key()
        );
        assert_ne!(
            ProviderPreset::OpenCodeZen.id(),
            ProviderPreset::OpenCodeGo.id()
        );
        assert_ne!(
            ProviderPreset::OpenCodeZen.default_base_url(),
            ProviderPreset::OpenCodeGo.default_base_url()
        );
    }

    #[test]
    fn xai_envs_never_alias() {
        assert_eq!(ProviderPreset::Xai.env_key(), Some("XAI_API_KEY"));
        assert_eq!(ProviderPreset::XaiOauth.env_key(), Some("XAI_OAUTH_TOKEN"));
        assert_ne!(
            ProviderPreset::Xai.env_key(),
            ProviderPreset::XaiOauth.env_key()
        );
    }

    #[test]
    fn auth_kinds_match_the_product() {
        assert_eq!(
            ProviderPreset::OpenAiCodex.auth_kind(),
            PresetAuthKind::OauthBrowser
        );
        assert_eq!(
            ProviderPreset::XaiOauth.auth_kind(),
            PresetAuthKind::OauthDevice
        );
        assert_eq!(
            ProviderPreset::OpenCodeGo.auth_kind(),
            PresetAuthKind::ApiKey
        );
        assert_eq!(ProviderPreset::Xai.auth_kind(), PresetAuthKind::ApiKey);
    }
}
