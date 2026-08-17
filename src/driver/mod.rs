//! Desktop proxy for the provider runtime owned by `waku-daemon`.

use crate::model::RuntimeEventCursor;

pub use waku_client::driver::{
    DriverEventSender, DriverHandle, DriverStartOptions, SessionOptions, event_channel,
};

pub(crate) fn start_remote(
    client: waku_client::DaemonClient,
    session_id: uuid::Uuid,
    options: DriverStartOptions,
    events: DriverEventSender,
) -> anyhow::Result<DriverHandle> {
    DriverHandle::start(client, session_id, options, events)
}

pub(crate) fn attach_remote(
    client: waku_client::DaemonClient,
    session_id: uuid::Uuid,
    runtime_id: uuid::Uuid,
    supports_steer: bool,
    replay_cursor: Option<RuntimeEventCursor>,
    events: DriverEventSender,
) -> anyhow::Result<DriverHandle> {
    DriverHandle::attach(
        client,
        session_id,
        runtime_id,
        supports_steer,
        replay_cursor,
        events,
    )
}
