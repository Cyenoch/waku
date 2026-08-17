//! Secret credential persistence. Production uses the OS keychain.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use waku_protocol::{ProviderId, SecretString};

use super::error::AuthError;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StoredCredential {
    ApiKey {
        key: String,
    },
    Oauth {
        access: String,
        refresh: String,
        expires_at_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    },
}

impl std::fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey { .. } => f.write_str("ApiKey([redacted])"),
            Self::Oauth {
                expires_at_ms,
                account_id,
                email,
                ..
            } => f
                .debug_struct("Oauth")
                .field("access", &"[redacted]")
                .field("refresh", &"[redacted]")
                .field("expires_at_ms", expires_at_ms)
                .field("account_id", account_id)
                .field("email", email)
                .finish(),
        }
    }
}

impl StoredCredential {
    pub fn api_key(key: SecretString) -> Self {
        Self::ApiKey {
            key: key.expose().to_owned(),
        }
    }
}

pub trait CredentialStore: Send + Sync {
    fn get(&self, provider: &ProviderId) -> Result<Option<StoredCredential>, AuthError>;
    fn set(&self, provider: &ProviderId, credential: StoredCredential) -> Result<(), AuthError>;
    fn delete(&self, provider: &ProviderId) -> Result<(), AuthError>;
}

#[derive(Default)]
pub struct MemoryCredentialStore {
    inner: Mutex<HashMap<String, StoredCredential>>,
}

impl CredentialStore for MemoryCredentialStore {
    fn get(&self, provider: &ProviderId) -> Result<Option<StoredCredential>, AuthError> {
        Ok(self.inner.lock().get(provider.as_str()).cloned())
    }

    fn set(&self, provider: &ProviderId, credential: StoredCredential) -> Result<(), AuthError> {
        self.inner
            .lock()
            .insert(provider.as_str().to_owned(), credential);
        Ok(())
    }

    fn delete(&self, provider: &ProviderId) -> Result<(), AuthError> {
        self.inner.lock().remove(provider.as_str());
        Ok(())
    }
}

#[cfg(any(test, not(target_os = "macos")))]
pub struct UnavailableCredentialStore {
    reason: &'static str,
}

#[cfg(any(test, not(target_os = "macos")))]
impl UnavailableCredentialStore {
    pub fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}

#[cfg(any(test, not(target_os = "macos")))]
impl CredentialStore for UnavailableCredentialStore {
    fn get(&self, _provider: &ProviderId) -> Result<Option<StoredCredential>, AuthError> {
        Err(AuthError::SecureStoreUnavailable(self.reason))
    }

    fn set(&self, _provider: &ProviderId, _credential: StoredCredential) -> Result<(), AuthError> {
        Err(AuthError::SecureStoreUnavailable(self.reason))
    }

    fn delete(&self, _provider: &ProviderId) -> Result<(), AuthError> {
        Err(AuthError::SecureStoreUnavailable(self.reason))
    }
}

#[cfg(target_os = "macos")]
pub struct KeychainCredentialStore {
    service: String,
}

#[cfg(target_os = "macos")]
impl KeychainCredentialStore {
    pub fn new() -> Self {
        Self {
            service: format!("{}.credentials", waku_protocol::identity::APP_ID),
        }
    }
}
#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

#[cfg(target_os = "macos")]
impl CredentialStore for KeychainCredentialStore {
    fn get(&self, provider: &ProviderId) -> Result<Option<StoredCredential>, AuthError> {
        match security_framework::passwords::get_generic_password(&self.service, provider.as_str())
        {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| AuthError::Store),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(_) => Err(AuthError::Store),
        }
    }

    fn set(&self, provider: &ProviderId, credential: StoredCredential) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(&credential).map_err(|_| AuthError::Store)?;
        let _ = security_framework::passwords::delete_generic_password(
            &self.service,
            provider.as_str(),
        );
        security_framework::passwords::set_generic_password(
            &self.service,
            provider.as_str(),
            &bytes,
        )
        .map_err(|_| AuthError::Store)
    }

    fn delete(&self, provider: &ProviderId) -> Result<(), AuthError> {
        match security_framework::passwords::delete_generic_password(
            &self.service,
            provider.as_str(),
        ) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(_) => Err(AuthError::Store),
        }
    }
}
pub fn production_store() -> Arc<dyn CredentialStore> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(KeychainCredentialStore::new())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(UnavailableCredentialStore::new(
            "Waku stores login secrets in the macOS Keychain",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_isolates_provider_ids() {
        let store = MemoryCredentialStore::default();
        store
            .set(
                &ProviderId::new("xai"),
                StoredCredential::api_key(SecretString::new("xai-key")),
            )
            .unwrap();
        store
            .set(
                &ProviderId::new("xai-oauth"),
                StoredCredential::Oauth {
                    access: "access-secret".into(),
                    refresh: "refresh-secret".into(),
                    expires_at_ms: 1,
                    account_id: None,
                    email: None,
                },
            )
            .unwrap();
        let xai = store.get(&ProviderId::new("xai")).unwrap().unwrap();
        let oauth = store.get(&ProviderId::new("xai-oauth")).unwrap().unwrap();
        assert!(!format!("{xai:?}").contains("xai-key"));
        assert!(!format!("{oauth:?}").contains("refresh-secret"));
        assert!(matches!(xai, StoredCredential::ApiKey { key } if key == "xai-key"));
        assert!(matches!(oauth, StoredCredential::Oauth { .. }));
    }

    #[test]
    fn unavailable_store_returns_explicit_login_error() {
        let store = UnavailableCredentialStore::new("no keychain");
        let err = store
            .set(
                &ProviderId::new("xai"),
                StoredCredential::api_key(SecretString::new("k")),
            )
            .unwrap_err();
        assert!(matches!(err, AuthError::SecureStoreUnavailable(_)));
    }
}
