#![recursion_limit = "256"]

//! Waku's daemon-side core.
//!
//! The core owns persistence, workspaces, generic terminal facilities,
//! and the embedded HTTP harness runtime. Provider-specific CLI
//! discovery and process adapters are intentionally absent.

rust_i18n::i18n!("../../locales", fallback = "en");

macro_rules! tr {
    ($key:expr) => {
        crate::i18n::translate($key)
    };
    ($key:expr, $($args:tt)*) => {
        rust_i18n::t!($key, $($args)*).into_owned()
    };
}

pub mod attachments;
pub mod auth;
pub mod blob_store;
pub mod checkpoint;
pub mod command_env;
pub mod composer_complete;
pub mod daemon;
pub mod driver;
pub mod git_branch;
pub mod git_commit;
pub mod i18n;
pub mod identity;
pub mod model;
pub mod persistence;
pub mod projectless;
pub mod protocol;
pub mod server;
pub mod settings;
pub mod skills;
pub mod terminal;
pub mod theme;
pub mod trajectory;
pub mod trajectory_detail;
pub mod trajectory_store;
pub mod usage;
pub mod usage_history;
pub mod workspace;
pub mod worktree;
pub use protocol::{
    APP_EXECUTABLE_ENV, ClientMessage, Command, DAEMON_ADDRESS_ENV, DAEMON_TOKEN_ENV, DaemonReady,
    PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome, ResponsePayload, RpcError,
    SaveTaskState, SequencedEvent, SequencedPayload, ServerMessage, StartTask, WireDriverEvent,
    WireDriverStartOptions, WireSessionOptions,
};
pub use server::{Backend, EventHub, EventSink, ServerOptions, serve};
pub use settings::{DaemonSettings, DaemonSettingsStore};
pub use workspace::{WorkspaceOperation, WorkspaceResult};
