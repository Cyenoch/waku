//! Rust transport and lifecycle support for clients of `wakuwaku-daemon`.
//!
//! This crate intentionally depends only on [`wakuwaku_protocol`], so GUI and CLI
//! clients cannot accidentally reach daemon-owned filesystem, Git, database,
//! or provider implementations.

mod client;
pub mod command_env;
pub mod composer_complete;
pub mod driver;
pub mod persistence;
mod process;
mod trajectory_client;
mod workspace_client;

pub use client::DaemonClient;
pub use driver::StartGenerationMismatch;
pub use process::{
    DEFAULT_EXPOSED_DAEMON_PORT, DaemonExposureSettings, DaemonProcess, DaemonSupervisor,
    parse_allowed_origins,
};
pub use trajectory_client::TrajectoryClient;
pub use wakuwaku_protocol::*;
pub use workspace_client::WorkspaceClient;

pub mod git_branch {
    pub use wakuwaku_protocol::git::{BranchEntry, BranchSnapshot};
}

pub mod git_commit {
    pub use wakuwaku_protocol::git::AgentInvocation;
    pub use wakuwaku_protocol::git::CommitSnapshot as Snapshot;
}

pub mod worktree {
    pub use wakuwaku_protocol::git::CreatedWorktree;
}
