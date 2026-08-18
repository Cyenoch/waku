//! Shared application identity used by the daemon and desktop client.

#[cfg(debug_assertions)]
pub const APP_NAME: &str = "WakuWaku Debug";
#[cfg(not(debug_assertions))]
pub const APP_NAME: &str = "Waku";

#[cfg(debug_assertions)]
pub const APP_ID: &str = "dev.bingzi.wakuwaku.dev";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "dev.bingzi.wakuwaku";

#[cfg(debug_assertions)]
pub const DATA_DIRECTORY_NAME: &str = "WakuWaku Debug";
#[cfg(not(debug_assertions))]
pub const DATA_DIRECTORY_NAME: &str = "Waku";

/// Workspace crate version used for Codex client_version/version headers.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
