//! Non-secret account metadata and catalog cache.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use wakuwaku_protocol::{
    CatalogSource, ExternalProvider, ModelCatalog, ModelCatalogEntry, ProviderId,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicAuthRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default)]
    pub relogin_required: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicAuthFile {
    #[serde(default)]
    pub accounts: BTreeMap<String, PublicAuthRecord>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogFile {
    #[serde(default)]
    pub catalogs: BTreeMap<String, ModelCatalog>,
}

pub struct AuthPersist {
    auth_path: PathBuf,
    catalog_path: PathBuf,
}

impl AuthPersist {
    pub fn new(directory: &Path) -> Self {
        Self {
            auth_path: directory.join("auth-status.json"),
            catalog_path: directory.join("model-catalog.json"),
        }
    }

    pub fn load_accounts(&self) -> PublicAuthFile {
        read_json(&self.auth_path).unwrap_or_default()
    }

    pub fn save_accounts(&self, file: &PublicAuthFile) -> io::Result<()> {
        write_json(&self.auth_path, file)
    }

    pub fn load_catalogs(&self) -> CatalogFile {
        read_json(&self.catalog_path).unwrap_or_default()
    }

    pub fn save_catalogs(&self, file: &CatalogFile) -> io::Result<()> {
        write_json(&self.catalog_path, file)
    }

    pub fn catalog_cache_key(provider: &ProviderId, endpoint: &ExternalProvider) -> String {
        format!(
            "{}|{}|{}",
            provider.as_str(),
            endpoint.base_url.trim(),
            endpoint.api_format.as_str()
        )
    }

    pub fn get_catalog(&self, key: &str) -> Option<ModelCatalog> {
        self.load_catalogs().catalogs.get(key).cloned()
    }

    pub fn put_catalog(
        &self,
        provider: &ProviderId,
        models: Vec<ModelCatalogEntry>,
        source: CatalogSource,
        fetched_at_ms: u64,
    ) -> io::Result<ModelCatalog> {
        self.put_catalog_at(provider.as_str(), provider, models, source, fetched_at_ms)
    }

    pub fn put_catalog_at(
        &self,
        key: &str,
        provider: &ProviderId,
        models: Vec<ModelCatalogEntry>,
        source: CatalogSource,
        fetched_at_ms: u64,
    ) -> io::Result<ModelCatalog> {
        let catalog = ModelCatalog {
            provider: provider.clone(),
            models,
            source,
            fetched_at_ms,
        };
        let mut file = self.load_catalogs();
        file.catalogs.insert(key.to_owned(), catalog.clone());
        self.save_catalogs(&file)?;
        Ok(catalog)
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)
}
