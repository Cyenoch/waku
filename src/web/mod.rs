//! Browser-only GPUI client for WakuWaku: a WebSocket transport over the
//! shared protocol plus a lightweight app entity. Compiled out of native
//! builds entirely.

mod app;
mod client;
mod reduce;

pub use app::WebApp;
