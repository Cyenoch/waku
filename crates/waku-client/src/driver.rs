//! Concrete RPC handle for the daemon-owned embedded runtime.

use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender, unbounded};
use uuid::Uuid;
use waku_protocol::model::{
    DriverEvent, InteractionMode, ProviderId, RuntimeEventCursor, RuntimeMode,
};
use waku_protocol::{PromptInput, encode_enum, event_from_wire};

use crate::DaemonClient;

#[derive(Clone)]
pub struct DriverEventSender {
    events: Sender<DriverEvent>,
    wake: smol::channel::Sender<()>,
}

impl DriverEventSender {
    pub fn send(&self, event: DriverEvent) -> bool {
        if self.events.send(event).is_err() {
            return false;
        }
        let _ = self.wake.try_send(());
        true
    }
}

pub fn event_channel(
    wake: smol::channel::Sender<()>,
) -> (DriverEventSender, Receiver<DriverEvent>) {
    let (events, receiver) = unbounded();
    (DriverEventSender { events, wake }, receiver)
}

pub struct DriverStartOptions {
    pub provider: ProviderId,
    pub cwd: PathBuf,
    pub mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<waku_protocol::ServiceTier>,
    pub context_window: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    pub mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<waku_protocol::ServiceTier>,
    pub context_window: Option<String>,
}

#[derive(Clone)]
pub struct DriverHandle {
    client: DaemonClient,
    session_id: Uuid,
    runtime_id: Uuid,
    supports_steer: bool,
    events: DriverEventSender,
}

impl DriverHandle {
    pub fn start(
        client: DaemonClient,
        session_id: Uuid,
        options: DriverStartOptions,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        let runtime_id = Uuid::new_v4();
        let command = waku_protocol::Command::Start {
            options: waku_protocol::WireDriverStartOptions {
                provider: options.provider.clone(),
                cwd: options.cwd,
                mode: encode_enum(options.mode)?,
                interaction_mode: encode_enum(options.interaction_mode)?,
                model: options.model,
                reasoning_effort: options.reasoning_effort,
                service_tier: options.service_tier,
                context_window: options.context_window,
            },
        };
        let supports_steer = match client.request(session_id, runtime_id, command) {
            Ok(waku_protocol::ResponsePayload::Started { supports_steer }) => supports_steer,
            Ok(_) => anyhow::bail!("Waku daemon returned an invalid start response"),
            Err(error) => return Err(error),
        };
        Self::connect(client, session_id, runtime_id, supports_steer, None, events)
    }

    pub fn attach(
        client: DaemonClient,
        session_id: Uuid,
        runtime_id: Uuid,
        supports_steer: bool,
        replay_cursor: Option<RuntimeEventCursor>,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        Self::connect(
            client,
            session_id,
            runtime_id,
            supports_steer,
            replay_cursor,
            events,
        )
    }

    fn connect(
        client: DaemonClient,
        session_id: Uuid,
        runtime_id: Uuid,
        supports_steer: bool,
        replay_cursor: Option<RuntimeEventCursor>,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        let remote_events = client.subscribe(session_id, runtime_id);
        let forwarding_events = events.clone();
        let thread_client = client.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("waku-daemon-session-{session_id}"))
            .spawn(move || {
                let mut saw_process_exit = false;
                while let Ok(sequenced) = remote_events.recv() {
                    if replay_cursor.is_some_and(|cursor| {
                        cursor.runtime_id == sequenced.runtime_id
                            && cursor.epoch == sequenced.epoch
                            && cursor.sequence >= sequenced.sequence
                    }) {
                        continue;
                    }
                    let cursor = RuntimeEventCursor {
                        runtime_id: sequenced.runtime_id,
                        epoch: sequenced.epoch,
                        sequence: sequenced.sequence,
                    };
                    let event = match event_from_wire(sequenced.event) {
                        Ok(event) => event,
                        Err(error) => DriverEvent::Error(format!(
                            "Waku daemon sent an invalid event: {error}"
                        )),
                    };
                    saw_process_exit |= matches!(&event, DriverEvent::ProcessExited);
                    if !forwarding_events.send(event)
                        || !forwarding_events.send(DriverEvent::RuntimeEventCursorAdvanced(cursor))
                    {
                        break;
                    }
                }
                thread_client.unsubscribe(session_id, runtime_id);
                if !saw_process_exit {
                    let _ = forwarding_events.send(DriverEvent::ProcessExited);
                }
            });
        if let Err(error) = spawn {
            client.unsubscribe(session_id, runtime_id);
            return Err(error.into());
        }
        Ok(Self {
            client,
            session_id,
            runtime_id,
            supports_steer,
            events,
        })
    }

    fn notify(&self, command: waku_protocol::Command) {
        if let Err(error) = self
            .client
            .notify(self.session_id, self.runtime_id, command)
        {
            let _ = self.events.send(DriverEvent::Error(format!(
                "Waku daemon command failed: {error}"
            )));
        }
    }

    pub fn prompt(&self, input: PromptInput) {
        self.notify(waku_protocol::Command::Prompt { input });
    }

    pub fn supports_steer(&self) -> bool {
        self.supports_steer
    }

    pub fn steer(&self, input: PromptInput) {
        self.notify(waku_protocol::Command::Steer { input });
    }

    pub fn cancel(&self) {
        self.notify(waku_protocol::Command::Cancel);
    }

    pub fn respond(&self, request_id: String, option_id: String) {
        self.notify(waku_protocol::Command::Respond {
            request_id,
            option_id,
        });
    }

    pub fn respond_user_input(
        &self,
        request_id: String,
        answers: Vec<waku_protocol::model::UserInputAnswer>,
    ) {
        self.notify(waku_protocol::Command::RespondUserInput {
            request_id,
            answers,
        });
    }

    pub fn refresh_background_work(&self) {
        self.notify(waku_protocol::Command::RefreshBackgroundWork);
    }

    pub fn stop_background_work(
        &self,
        key: waku_protocol::model::BackgroundWorkKey,
        control_id: String,
    ) {
        self.notify(waku_protocol::Command::StopBackgroundWork { key, control_id });
    }

    pub fn apply_options(&self, options: SessionOptions) -> bool {
        let options = (|| {
            Ok::<_, anyhow::Error>(waku_protocol::WireSessionOptions {
                mode: encode_enum(options.mode)?,
                interaction_mode: encode_enum(options.interaction_mode)?,
                model: options.model,
                reasoning_effort: options.reasoning_effort,
                service_tier: options.service_tier,
                context_window: options.context_window,
            })
        })();
        let Ok(options) = options else {
            return false;
        };
        matches!(
            self.client.request(
                self.session_id,
                self.runtime_id,
                waku_protocol::Command::ApplyOptions { options }
            ),
            Ok(waku_protocol::ResponsePayload::OptionsApplied { applied: true })
        )
    }

    pub fn rollback(&self, turns: usize) -> anyhow::Result<()> {
        match self.client.request(
            self.session_id,
            self.runtime_id,
            waku_protocol::Command::Rollback { turns },
        )? {
            waku_protocol::ResponsePayload::Ack => Ok(()),
            _ => anyhow::bail!("Waku daemon returned an invalid rollback response"),
        }
    }

    pub fn fork(&self, turns_to_remove: usize) -> anyhow::Result<()> {
        match self.client.request(
            self.session_id,
            self.runtime_id,
            waku_protocol::Command::Fork { turns_to_remove },
        )? {
            waku_protocol::ResponsePayload::Ack => Ok(()),
            _ => anyhow::bail!("Waku daemon returned an invalid fork response"),
        }
    }

    pub fn close(&self) {
        self.notify(waku_protocol::Command::CloseSession);
    }
}

impl Drop for DriverHandle {
    fn drop(&mut self) {
        self.client.unsubscribe(self.session_id, self.runtime_id);
    }
}
