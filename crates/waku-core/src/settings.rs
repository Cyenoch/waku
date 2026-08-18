//! Daemon-owned, user-editable configuration.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use uuid::Uuid;
pub use waku_protocol::settings::DaemonSettings;

pub struct DaemonSettingsStore {
    path: PathBuf,
    settings: Mutex<DaemonSettings>,
}

impl DaemonSettingsStore {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let settings = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(settings) => settings,
                Err(error) => {
                    let backup = quarantine_corrupt_settings(&path)?;
                    eprintln!(
                        "Waku daemon moved invalid settings to {}: {error}",
                        backup.display()
                    );
                    let settings = DaemonSettings::default();
                    write_atomic(&path, &settings)?;
                    settings
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => DaemonSettings::default(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            settings: Mutex::new(settings),
        })
    }

    pub fn get(&self) -> DaemonSettings {
        self.settings.lock().clone()
    }

    pub fn replace(&self, settings: DaemonSettings) -> io::Result<()> {
        settings
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut current = self.settings.lock();
        write_atomic(&self.path, &settings)?;
        *current = settings;
        Ok(())
    }
}

fn quarantine_corrupt_settings(path: &Path) -> io::Result<PathBuf> {
    let extension = format!("json.corrupt-{}", Uuid::new_v4().simple());
    let backup = path.with_extension(extension);
    fs::rename(path, &backup)?;
    Ok(backup)
}

fn write_atomic(path: &Path, settings: &DaemonSettings) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(settings).map_err(to_io_error)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, data)?;
    fs::rename(temporary, path)
}

fn to_io_error(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_current_settings_are_quarantined_and_replaced() {
        let directory = std::env::temp_dir().join(format!("waku-settings-{}", Uuid::new_v4()));
        let path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, b"{ definitely not json").unwrap();

        let store = DaemonSettingsStore::open(path.clone()).unwrap();

        assert_eq!(store.get(), DaemonSettings::default());
        assert!(serde_json::from_slice::<DaemonSettings>(&fs::read(&path).unwrap()).is_ok());
        assert!(fs::read_dir(&directory).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("settings.json.corrupt-")
        }));
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn replace_rejects_reserved_and_invalid_endpoints() {
        let directory = std::env::temp_dir().join(format!("waku-settings-{}", Uuid::new_v4()));
        let path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        let store = DaemonSettingsStore::open(path).unwrap();

        let reserved = DaemonSettings {
            external_providers: vec![waku_protocol::ExternalProvider::new(
                waku_protocol::ProviderId::OPENAI_CHAT,
                "Nope",
                "http://127.0.0.1:9/v1",
                waku_protocol::ApiFormat::OpenAiChat,
                "gpt-5",
            )],
            extra: Default::default(),
        };
        let error = store.replace(reserved).unwrap_err();
        assert!(error.to_string().contains("reserved"), "{error}");

        let bad_url = DaemonSettings {
            external_providers: vec![waku_protocol::ExternalProvider::new(
                "corp-chat",
                "Corp",
                "not-a-url",
                waku_protocol::ApiFormat::OpenAiChat,
                "gpt-5",
            )],
            extra: Default::default(),
        };
        assert!(store.replace(bad_url).is_err());
        assert!(store.get().external_providers.is_empty());
        fs::remove_dir_all(directory).ok();
    }
}
