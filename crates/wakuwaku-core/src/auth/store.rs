//! Secret credential persistence. Production uses a daemon-owned local file.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use wakuwaku_protocol::{ProviderId, SecretString};

use super::error::AuthError;

/// File name of the daemon-owned credential store under the app data directory.
pub const CREDENTIALS_FILE_NAME: &str = "credentials.json";

const CREDENTIAL_FILE_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
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

#[cfg(test)]
pub struct UnavailableCredentialStore {
    reason: &'static str,
}

#[cfg(test)]
impl UnavailableCredentialStore {
    pub fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}

#[cfg(test)]
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

/// Versioned on-disk envelope. `BTreeMap` keeps serialization deterministic.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    version: u32,
    credentials: BTreeMap<String, StoredCredential>,
}

#[derive(Serialize)]
struct CredentialFileRef<'a> {
    version: u32,
    credentials: &'a BTreeMap<String, StoredCredential>,
}

/// Daemon-owned credential file under the application data directory.
///
/// On Unix the parent directory is owner-only (`0700`) and the file is owner
/// read/write (`0600`). A missing file is an empty store. Malformed contents
/// and unsafe permissions fail closed and never appear in `Debug`.
pub struct FileCredentialStore {
    path: PathBuf,
    inner: Mutex<BTreeMap<String, StoredCredential>>,
}

impl std::fmt::Debug for FileCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("FileCredentialStore")
            .field("path", &self.path)
            .field("entries", &inner.len())
            .finish()
    }
}

impl FileCredentialStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AuthError> {
        let path = path.into();
        let credentials = load_credentials(&path)?;
        Ok(Self {
            path,
            inner: Mutex::new(credentials),
        })
    }
}

impl CredentialStore for FileCredentialStore {
    fn get(&self, provider: &ProviderId) -> Result<Option<StoredCredential>, AuthError> {
        Ok(self.inner.lock().get(provider.as_str()).cloned())
    }

    fn set(&self, provider: &ProviderId, credential: StoredCredential) -> Result<(), AuthError> {
        let mut inner = self.inner.lock();
        let previous = inner.insert(provider.as_str().to_owned(), credential);
        if let Err(error) = persist_credentials(&self.path, &inner) {
            match previous {
                Some(previous) => {
                    inner.insert(provider.as_str().to_owned(), previous);
                }
                None => {
                    inner.remove(provider.as_str());
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn delete(&self, provider: &ProviderId) -> Result<(), AuthError> {
        let mut inner = self.inner.lock();
        let Some(previous) = inner.remove(provider.as_str()) else {
            return Ok(());
        };
        if let Err(error) = persist_credentials(&self.path, &inner) {
            inner.insert(provider.as_str().to_owned(), previous);
            return Err(error);
        }
        Ok(())
    }
}

pub fn production_store(directory: &Path) -> Result<Arc<dyn CredentialStore>, AuthError> {
    Ok(Arc::new(FileCredentialStore::open(
        directory.join(CREDENTIALS_FILE_NAME),
    )?))
}

fn load_credentials(path: &Path) -> Result<BTreeMap<String, StoredCredential>, AuthError> {
    if let Some(parent) = parent_dir(path) {
        prepare_parent_dir(parent)?;
    }
    match fs::metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(_) => Err(AuthError::Store),
        Ok(metadata) => {
            #[cfg(unix)]
            {
                if !is_owner_secret_file(&metadata) {
                    return Err(AuthError::SecureStoreUnavailable(
                        "credential file permissions are unsafe",
                    ));
                }
            }
            let bytes = fs::read(path).map_err(|_| AuthError::Store)?;
            let file: CredentialFile =
                serde_json::from_slice(&bytes).map_err(|_| AuthError::Store)?;
            if file.version != CREDENTIAL_FILE_VERSION {
                return Err(AuthError::Store);
            }
            Ok(file.credentials)
        }
    }
}

fn persist_credentials(
    path: &Path,
    credentials: &BTreeMap<String, StoredCredential>,
) -> Result<(), AuthError> {
    if let Some(parent) = parent_dir(path) {
        prepare_parent_dir(parent)?;
    }
    let bytes = serde_json::to_vec(&CredentialFileRef {
        version: CREDENTIAL_FILE_VERSION,
        credentials,
    })
    .map_err(|_| AuthError::Store)?;
    let temporary = path.with_extension("json.tmp");
    write_secret_file(&temporary, &bytes)?;
    replace_file(&temporary, path)?;
    #[cfg(unix)]
    {
        let metadata = fs::metadata(path).map_err(|_| AuthError::Store)?;
        if !is_owner_secret_file(&metadata) {
            return Err(AuthError::SecureStoreUnavailable(
                "credential file permissions are unsafe",
            ));
        }
    }
    Ok(())
}

fn replace_file(temporary: &Path, path: &Path) -> Result<(), AuthError> {
    #[cfg(windows)]
    {
        if path.exists() {
            fs::remove_file(path).map_err(|_| AuthError::Store)?;
        }
    }
    match fs::rename(temporary, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = fs::remove_file(temporary);
            Err(AuthError::Store)
        }
    }
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| AuthError::Store)?;
    file.write_all(bytes).map_err(|_| AuthError::Store)?;
    file.sync_all().map_err(|_| AuthError::Store)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = file.metadata().map_err(|_| AuthError::Store)?.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions).map_err(|_| AuthError::Store)?;
    }
    Ok(())
}

fn prepare_parent_dir(parent: &Path) -> Result<(), AuthError> {
    match fs::metadata(parent) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_owner_only_dir(parent)?;
        }
        Err(_) => return Err(AuthError::Store),
        Ok(metadata) if !metadata.is_dir() => return Err(AuthError::Store),
        Ok(_) => {}
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(parent).map_err(|_| AuthError::Store)?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(parent, permissions).map_err(|_| AuthError::Store)?;
        let metadata = fs::metadata(parent).map_err(|_| AuthError::Store)?;
        if !is_owner_only_dir(&metadata) {
            return Err(AuthError::SecureStoreUnavailable(
                "credential directory permissions are unsafe",
            ));
        }
    }
    Ok(())
}

fn create_owner_only_dir(parent: &Path) -> Result<(), AuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| AuthError::Store)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(parent).map_err(|_| AuthError::Store)
    }
}

fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

#[cfg(unix)]
fn permission_bits(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(unix)]
fn is_owner_only_dir(metadata: &fs::Metadata) -> bool {
    metadata.is_dir() && permission_bits(metadata) == 0o700
}

#[cfg(unix)]
fn is_owner_secret_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && permission_bits(metadata) == 0o600
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempCredDir {
        dir: PathBuf,
    }

    impl TempCredDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wakuwaku-cred-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = fs::metadata(&dir).unwrap().permissions();
                permissions.set_mode(0o700);
                fs::set_permissions(&dir, permissions).unwrap();
            }
            Self { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.join(CREDENTIALS_FILE_NAME)
        }
    }

    impl Drop for TempCredDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn sample_oauth() -> StoredCredential {
        StoredCredential::Oauth {
            access: "access-secret".into(),
            refresh: "refresh-secret".into(),
            expires_at_ms: 1,
            account_id: Some("acct".into()),
            email: Some("user@example.com".into()),
        }
    }

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
            .set(&ProviderId::new("xai-oauth"), sample_oauth())
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
        let store = UnavailableCredentialStore::new("unavailable");
        let err = store
            .set(
                &ProviderId::new("xai"),
                StoredCredential::api_key(SecretString::new("k")),
            )
            .unwrap_err();
        assert!(matches!(err, AuthError::SecureStoreUnavailable(_)));
    }

    #[test]
    fn file_store_round_trips_across_instances() {
        let temp = TempCredDir::new();
        let path = temp.path();
        let store = FileCredentialStore::open(&path).unwrap();
        store
            .set(
                &ProviderId::new("xai"),
                StoredCredential::api_key(SecretString::new("xai-key")),
            )
            .unwrap();
        store
            .set(&ProviderId::new("xai-oauth"), sample_oauth())
            .unwrap();
        drop(store);

        let store = FileCredentialStore::open(&path).unwrap();
        let xai = store.get(&ProviderId::new("xai")).unwrap().unwrap();
        let oauth = store.get(&ProviderId::new("xai-oauth")).unwrap().unwrap();
        assert!(matches!(xai, StoredCredential::ApiKey { key } if key == "xai-key"));
        assert!(
            matches!(oauth, StoredCredential::Oauth { refresh, expires_at_ms, .. } if refresh == "refresh-secret" && expires_at_ms == 1)
        );
    }

    #[test]
    fn file_store_replaces_and_deletes_provider() {
        let temp = TempCredDir::new();
        let path = temp.path();
        let store = FileCredentialStore::open(&path).unwrap();
        store
            .set(
                &ProviderId::new("xai"),
                StoredCredential::api_key(SecretString::new("first")),
            )
            .unwrap();
        store
            .set(
                &ProviderId::new("xai"),
                StoredCredential::api_key(SecretString::new("second")),
            )
            .unwrap();
        let current = store.get(&ProviderId::new("xai")).unwrap().unwrap();
        assert!(matches!(current, StoredCredential::ApiKey { key } if key == "second"));
        store.delete(&ProviderId::new("xai")).unwrap();
        assert!(store.get(&ProviderId::new("xai")).unwrap().is_none());
        drop(store);

        let store = FileCredentialStore::open(&path).unwrap();
        assert!(store.get(&ProviderId::new("xai")).unwrap().is_none());
    }

    #[test]
    fn file_store_missing_file_is_empty_and_uncreated() {
        let temp = TempCredDir::new();
        let path = temp.path();
        let store = FileCredentialStore::open(&path).unwrap();
        assert!(store.get(&ProviderId::new("xai")).unwrap().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn file_store_fails_closed_on_malformed_file() {
        let temp = TempCredDir::new();
        let path = temp.path();
        fs::write(&path, "{not-json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(&path, permissions).unwrap();
        }
        let error = FileCredentialStore::open(&path).unwrap_err();
        assert!(matches!(error, AuthError::Store));
        assert!(!error.to_string().contains("not-json"));
    }

    #[test]
    fn file_store_debug_redacts_secrets() {
        let temp = TempCredDir::new();
        let store = FileCredentialStore::open(temp.path()).unwrap();
        store
            .set(
                &ProviderId::new("xai"),
                StoredCredential::api_key(SecretString::new("super-secret-key")),
            )
            .unwrap();
        store
            .set(&ProviderId::new("xai-oauth"), sample_oauth())
            .unwrap();
        let rendered = format!("{store:?}");
        assert!(!rendered.contains("super-secret-key"));
        assert!(!rendered.contains("access-secret"));
        assert!(!rendered.contains("refresh-secret"));
        let credential = store.get(&ProviderId::new("xai")).unwrap().unwrap();
        assert!(!format!("{credential:?}").contains("super-secret-key"));
    }

    #[cfg(unix)]
    #[test]
    fn file_store_writes_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempCredDir::new();
        let path = temp.path();
        let store = FileCredentialStore::open(&path).unwrap();
        store
            .set(
                &ProviderId::new("xai"),
                StoredCredential::api_key(SecretString::new("xai-key")),
            )
            .unwrap();
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(&temp.dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn file_store_fails_closed_on_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempCredDir::new();
        let path = temp.path();
        let payload = serde_json::json!({
            "version": 1,
            "credentials": {
                "xai": { "type": "apiKey", "key": "leaked-secret" }
            }
        });
        fs::write(&path, payload.to_string()).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&path, permissions).unwrap();
        let error = FileCredentialStore::open(&path).unwrap_err();
        assert!(matches!(error, AuthError::SecureStoreUnavailable(_)));
        assert!(!error.to_string().contains("leaked-secret"));
    }
}
