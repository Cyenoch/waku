//! Desktop proxy for the provider runtime owned by `wakuwaku-daemon`.

use crate::model::RuntimeEventCursor;

pub use wakuwaku_client::driver::{
    DriverEventSender, DriverHandle, DriverStartOptions, SessionOptions, event_channel,
};

pub(crate) fn start_remote(
    client: wakuwaku_client::DaemonClient,
    session_id: uuid::Uuid,
    options: DriverStartOptions,
    task: Option<wakuwaku_client::StartTask>,
    events: DriverEventSender,
) -> anyhow::Result<DriverHandle> {
    DriverHandle::start_restoring(client, session_id, options, task, events)
}

pub(crate) fn attach_remote(
    client: wakuwaku_client::DaemonClient,
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
