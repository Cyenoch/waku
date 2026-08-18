//! Pure daemon-path classification shared by clients and the daemon.

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

fn workspace_root_slot() -> &'static RwLock<Option<PathBuf>> {
    static ROOT: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
    ROOT.get_or_init(|| {
        RwLock::new(dirs::home_dir().map(|home| home.join(".wakuwaku").join("projects")))
    })
}

pub fn set_workspace_root(root: Option<PathBuf>) {
    if let Ok(mut current) = workspace_root_slot().write() {
        *current = root;
    }
}

pub fn workspace_root() -> Option<PathBuf> {
    workspace_root_slot().read().ok()?.clone()
}

pub fn home_directory() -> Option<PathBuf> {
    let root = workspace_root()?;
    root.parent()?.parent().map(Path::to_path_buf)
}

pub fn is_projectless_path(path: &Path) -> bool {
    workspace_root().is_some_and(|root| path.starts_with(&root))
}
