//! Auth errors. Never include secret material in Display.

use wakuwaku_protocol::ProviderId;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("credential store is unavailable: {0}")]
    SecureStoreUnavailable(&'static str),
    #[error("credential store failed")]
    Store,
    #[error("login was cancelled")]
    Cancelled,
    #[error("login is not in progress")]
    NoActiveLogin,
    #[error("provider {0} requires sign-in")]
    ReloginRequired(ProviderId),
    #[error("{0}")]
    Failed(String),
}

impl AuthError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}
