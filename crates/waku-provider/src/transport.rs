//! Request transport profile and per-model capabilities.
//!
//! `ApiFormat` is the content dialect. `TransportProfile` is how that dialect
//! is addressed (URL/headers/body omissions). Auth identity is neither.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ApiFormat;

/// How a request is addressed on the wire, independent of content format.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TransportProfile {
    #[default]
    Standard,
    /// ChatGPT Codex backend: `/codex/responses`, subscription headers,
    /// no sampling parameters.
    Codex,
}

impl TransportProfile {
    pub const ALL: [Self; 2] = [Self::Standard, Self::Codex];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Codex => "codex",
        }
    }
}

impl std::fmt::Display for TransportProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a selected model accepts on its native request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
    pub service_tier: bool,
    pub reasoning_effort: bool,
    pub reasoning_summary: bool,
    pub sampling: bool,
}

impl ModelCapabilities {
    /// Official OpenAI API (api.openai.com). Service tier is provider-native.
    pub const fn openai_api(format: ApiFormat) -> Self {
        Self {
            service_tier: matches!(format, ApiFormat::OpenAiResponses | ApiFormat::OpenAiChat),
            reasoning_effort: matches!(format, ApiFormat::OpenAiResponses | ApiFormat::OpenAiChat),
            reasoning_summary: matches!(format, ApiFormat::OpenAiResponses),
            sampling: true,
        }
    }

    /// OpenAI-compatible third-party dialect. Service tier is not guaranteed.
    pub const fn openai_compatible(format: ApiFormat) -> Self {
        Self {
            service_tier: false,
            reasoning_effort: matches!(format, ApiFormat::OpenAiResponses | ApiFormat::OpenAiChat),
            reasoning_summary: false,
            sampling: true,
        }
    }

    pub const fn custom(format: ApiFormat) -> Self {
        Self::openai_compatible(format)
    }

    pub const fn codex() -> Self {
        Self {
            // ChatGPT Codex backend has no grounded service_tier field.
            service_tier: false,
            reasoning_effort: true,
            reasoning_summary: true,
            sampling: false,
        }
    }

    pub const fn xai(reasoning_effort: bool) -> Self {
        Self {
            service_tier: false,
            reasoning_effort,
            reasoning_summary: false,
            sampling: true,
        }
    }

    pub const fn anthropic() -> Self {
        Self {
            service_tier: false,
            reasoning_effort: true,
            reasoning_summary: false,
            sampling: true,
        }
    }
}

/// Why a discovered model is hidden from the default picker.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum UnsupportedReason {
    GoogleFormat,
    Unroutable,
    NonChat,
}

impl UnsupportedReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GoogleFormat => "google-format",
            Self::Unroutable => "unroutable",
            Self::NonChat => "non-chat",
        }
    }
}

/// Where a catalog snapshot came from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum CatalogSource {
    Live,
    Cache,
    Seed,
}

impl CatalogSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Cache => "cache",
            Self::Seed => "seed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_tier_is_openai_api_only() {
        assert!(ModelCapabilities::openai_api(ApiFormat::OpenAiResponses).service_tier);
        assert!(ModelCapabilities::openai_api(ApiFormat::OpenAiChat).service_tier);
        assert!(!ModelCapabilities::openai_compatible(ApiFormat::OpenAiResponses).service_tier);
        assert!(!ModelCapabilities::codex().service_tier);
        assert!(!ModelCapabilities::xai(true).service_tier);
        assert!(!ModelCapabilities::custom(ApiFormat::OpenAiChat).service_tier);
        assert!(!ModelCapabilities::anthropic().service_tier);
    }
}
