//! Production OAuth and discovery URLs. Tests inject a private copy.

/// Closed set of login/token hosts. Not user configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthEndpoints {
    pub openai_authorize: String,
    pub openai_token: String,
    pub openai_device_usercode: String,
    pub openai_device_token: String,
    pub openai_device_auth_url: String,
    pub openai_device_redirect: String,
    pub xai_discovery: String,
    pub xai_device_code: String,
    pub xai_userinfo: String,
    /// Extra hosts allowed for xAI token_endpoint. Empty means production pin.
    pub xai_allowed_token_hosts: Vec<String>,
}

impl AuthEndpoints {
    pub const CODEX_CLIENT_ID: &'static str = "app_EMoamEEZ73f0CkXaXp7hrann";
    pub const CODEX_CALLBACK_HOST: &'static str = "127.0.0.1";
    pub const CODEX_CALLBACK_PORT: u16 = 1455;
    pub const CODEX_CALLBACK_PATH: &'static str = "/auth/callback";
    pub const CODEX_SCOPE: &'static str =
        "openid profile email offline_access api.connectors.read api.connectors.invoke";
    pub const CODEX_ORIGINATOR: &'static str = "waku";

    pub const fn client_version() -> &'static str {
        concat!("waku-", env!("CARGO_PKG_VERSION"))
    }
    pub const XAI_OAUTH_CLIENT_ID: &'static str = "b1a00492-073a-47ea-816f-4c329264a828";
    pub const XAI_OAUTH_SCOPE: &'static str =
        "openid profile email offline_access grok-cli:access api:access";

    pub fn production() -> Self {
        Self {
            openai_authorize: "https://auth.openai.com/oauth/authorize".into(),
            openai_token: "https://auth.openai.com/oauth/token".into(),
            openai_device_usercode: "https://auth.openai.com/api/accounts/deviceauth/usercode"
                .into(),
            openai_device_token: "https://auth.openai.com/api/accounts/deviceauth/token".into(),
            openai_device_auth_url: "https://auth.openai.com/codex/device".into(),
            openai_device_redirect: "https://auth.openai.com/deviceauth/callback".into(),
            xai_discovery: "https://auth.x.ai/.well-known/openid-configuration".into(),
            xai_device_code: "https://auth.x.ai/oauth2/device/code".into(),
            xai_userinfo: "https://auth.x.ai/oauth2/userinfo".into(),
            xai_allowed_token_hosts: Vec::new(),
        }
    }

    pub fn allows_xai_token_endpoint(&self, url: &str) -> bool {
        if self.xai_allowed_token_hosts.is_empty() {
            return is_pinned_xai_token_endpoint(url);
        }
        let Ok(parsed) = url::Url::parse(url.trim()) else {
            return false;
        };
        parsed.host_str().is_some_and(|host| {
            self.xai_allowed_token_hosts
                .iter()
                .any(|allowed| host == allowed || host.ends_with(&format!(".{allowed}")))
        })
    }

    pub fn codex_callback_uri() -> String {
        format!(
            "http://localhost:{}{}",
            Self::CODEX_CALLBACK_PORT,
            Self::CODEX_CALLBACK_PATH
        )
    }
}

/// True when a discovered xAI token endpoint may receive a refresh or poll.
pub fn is_pinned_xai_token_endpoint(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url.trim()) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    parsed
        .host_str()
        .is_some_and(|host| host == "x.ai" || host.ends_with(".x.ai"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_hosts_are_the_documented_ones() {
        let endpoints = AuthEndpoints::production();
        assert_eq!(
            endpoints.openai_authorize,
            "https://auth.openai.com/oauth/authorize"
        );
        assert_eq!(
            endpoints.xai_device_code,
            "https://auth.x.ai/oauth2/device/code"
        );
        assert_eq!(
            AuthEndpoints::codex_callback_uri(),
            "http://localhost:1455/auth/callback"
        );
    }

    #[test]
    fn xai_token_endpoint_pin_rejects_foreign_hosts() {
        assert!(is_pinned_xai_token_endpoint(
            "https://auth.x.ai/oauth2/token"
        ));
        assert!(!is_pinned_xai_token_endpoint(
            "https://evil.example/oauth2/token"
        ));
        assert!(!is_pinned_xai_token_endpoint(
            "http://auth.x.ai/oauth2/token"
        ));
    }
}
