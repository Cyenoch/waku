//! Serializable auth status and login phases. No tokens live here.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use crate::{CatalogSource, ModelCatalogEntry, ProviderId};

/// Login method requested by a client.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum LoginMethod {
    ApiKey,
    OauthBrowser,
    OauthDevice,
}

/// How the daemon currently authenticates a provider.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum AuthMethod {
    None,
    EnvKey,
    StoredApiKey,
    Oauth,
}

impl AuthMethod {
    pub const fn is_connected(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Long-running login state machine as seen on the wire.
///
/// Every active or terminal phase carries both `login_id` and `provider` so a
/// client can attribute `GetAuthStatus` results after reconnect without a
/// side map. [`Self::Idle`] is neither active nor terminal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AuthPhase {
    Idle,
    AwaitingBrowser {
        login_id: Uuid,
        provider: ProviderId,
        url: String,
    },
    AwaitingDevice {
        login_id: Uuid,
        provider: ProviderId,
        user_code: String,
        verification_url: String,
        instructions: String,
    },
    AwaitingApiKey {
        login_id: Uuid,
        provider: ProviderId,
        instructions: String,
    },
    Completed {
        login_id: Uuid,
        provider: ProviderId,
    },
    Failed {
        login_id: Uuid,
        provider: ProviderId,
        message: String,
    },
}

impl AuthPhase {
    pub fn login_id(&self) -> Option<Uuid> {
        match self {
            Self::Idle => None,
            Self::AwaitingBrowser { login_id, .. }
            | Self::AwaitingDevice { login_id, .. }
            | Self::AwaitingApiKey { login_id, .. }
            | Self::Completed { login_id, .. }
            | Self::Failed { login_id, .. } => Some(*login_id),
        }
    }

    pub fn provider(&self) -> Option<&ProviderId> {
        match self {
            Self::Idle => None,
            Self::AwaitingBrowser { provider, .. }
            | Self::AwaitingDevice { provider, .. }
            | Self::AwaitingApiKey { provider, .. }
            | Self::Completed { provider, .. }
            | Self::Failed { provider, .. } => Some(provider),
        }
    }
}

/// Public, secret-free account view for one provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAuthStatus {
    pub provider: ProviderId,
    pub method: AuthMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    pub relogin_required: bool,
}

impl ProviderAuthStatus {
    pub const fn is_connected(&self) -> bool {
        self.method.is_connected()
    }
}

/// Catalog snapshot returned to clients.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub provider: ProviderId,
    pub models: Vec<ModelCatalogEntry>,
    pub source: CatalogSource,
    pub fetched_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_phase_wire_tags_are_camel_case() {
        let phase = AuthPhase::AwaitingDevice {
            login_id: Uuid::nil(),
            provider: ProviderId::new("xai-oauth"),
            user_code: "ABCD".into(),
            verification_url: "https://auth.x.ai/device".into(),
            instructions: "Enter code".into(),
        };
        let json = serde_json::to_value(&phase).unwrap();
        assert_eq!(json["type"], "awaitingDevice");
        assert_eq!(json["userCode"], "ABCD");
        assert_eq!(json["provider"], "xai-oauth");
        assert_eq!(json["loginId"], Uuid::nil().to_string());
        assert!(json.get("access").is_none());
        assert!(json.get("refresh").is_none());
    }

    #[test]
    fn active_and_terminal_phases_are_self_describing() {
        let provider = ProviderId::new("xai");
        let phase = AuthPhase::AwaitingApiKey {
            login_id: Uuid::nil(),
            provider: provider.clone(),
            instructions: "Paste the API key".into(),
        };
        assert_eq!(phase.login_id(), Some(Uuid::nil()));
        assert_eq!(phase.provider(), Some(&provider));

        let failed = AuthPhase::Failed {
            login_id: Uuid::nil(),
            provider: ProviderId::new("openai-codex"),
            message: "device login".into(),
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["type"], "failed");
        assert_eq!(json["loginId"], Uuid::nil().to_string());
        assert_eq!(json["provider"], "openai-codex");
        assert!(json.get("login_id").is_none());
        assert_eq!(failed.login_id(), Some(Uuid::nil()));
        assert_eq!(
            failed.provider().map(ProviderId::as_str),
            Some("openai-codex")
        );
        assert!(AuthPhase::Idle.login_id().is_none());
        assert!(AuthPhase::Idle.provider().is_none());
    }

    #[test]
    fn stored_and_oauth_methods_count_as_connected() {
        assert!(AuthMethod::EnvKey.is_connected());
        assert!(AuthMethod::StoredApiKey.is_connected());
        assert!(AuthMethod::Oauth.is_connected());
        assert!(!AuthMethod::None.is_connected());
    }

    #[test]
    fn auth_status_connected_follows_method() {
        let status = ProviderAuthStatus {
            provider: ProviderId::new("opencode-go"),
            method: AuthMethod::StoredApiKey,
            email: None,
            account_id: None,
            expires_at_ms: None,
            relogin_required: false,
        };
        assert!(status.is_connected());
    }
}
