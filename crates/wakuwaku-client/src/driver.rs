//! Concrete RPC handle for the daemon-owned embedded runtime.

use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use uuid::Uuid;
use wakuwaku_protocol::model::{
    DriverEvent, InteractionMode, ProviderId, RuntimeEventCursor, RuntimeMode,
};
use wakuwaku_protocol::{PromptInput, SequencedPayload, encode_enum, event_from_wire};

use crate::DaemonClient;

/// Daemon accepted a different task generation than the client submitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartGenerationMismatch {
    pub submitted: u64,
    pub accepted: Option<u64>,
}

impl std::fmt::Display for StartGenerationMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.accepted {
            Some(accepted) => write!(
                f,
                "start task generation {} does not match daemon transcript {accepted}",
                self.submitted
            ),
            None => write!(
                f,
                "start task generation {} has no daemon transcript generation",
                self.submitted
            ),
        }
    }
}

impl std::error::Error for StartGenerationMismatch {}

/// Compare the generation the client submitted with the generation Start returned.
///
/// A restore payload must land on the same transcript the daemon kept. A start
/// without a payload has no restore race and is accepted as-is.
pub fn accept_start_generation(
    submitted: Option<u64>,
    accepted: Option<u64>,
) -> Result<Option<u64>, StartGenerationMismatch> {
    match (submitted, accepted) {
        (None, accepted) => Ok(accepted),
        (Some(submitted), Some(accepted)) if submitted == accepted => Ok(Some(accepted)),
        (Some(submitted), accepted) => Err(StartGenerationMismatch {
            submitted,
            accepted,
        }),
    }
}

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
    pub service_tier: Option<wakuwaku_protocol::ServiceTier>,
    pub context_window: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    pub mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<wakuwaku_protocol::ServiceTier>,
    pub context_window: Option<String>,
}

struct DriverHandleInner {
    client: DaemonClient,
    session_id: Uuid,
    runtime_id: Uuid,
    supports_steer: bool,
    events: DriverEventSender,
}

/// RPC handle for one daemon-owned runtime.
///
/// Clones share the event subscription. Unsubscribing from every `Drop` would
/// cut the stream the first time a temporary clone went out of scope — the
/// desktop submit path clones the handle to `prompt` and then drops it.
#[derive(Clone)]
pub struct DriverHandle {
    inner: Arc<DriverHandleInner>,
}

impl DriverHandle {
    pub fn start(
        client: DaemonClient,
        session_id: Uuid,
        options: DriverStartOptions,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        Self::start_restoring(client, session_id, options, None, events)
    }

    pub fn start_restoring(
        client: DaemonClient,
        session_id: Uuid,
        options: DriverStartOptions,
        task: Option<wakuwaku_protocol::StartTask>,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        let submitted = task.as_ref().map(|task| task.generation);
        let runtime_id = Uuid::new_v4();
        let command = wakuwaku_protocol::Command::Start {
            options: wakuwaku_protocol::WireDriverStartOptions {
                provider: options.provider.clone(),
                cwd: options.cwd,
                mode: encode_enum(options.mode)?,
                interaction_mode: encode_enum(options.interaction_mode)?,
                model: options.model,
                reasoning_effort: options.reasoning_effort,
                service_tier: options.service_tier,
                context_window: options.context_window,
                task: task.map(Box::new),
            },
        };
        let supports_steer = match client.request(session_id, runtime_id, command) {
            Ok(wakuwaku_protocol::ResponsePayload::Started {
                supports_steer,
                task_generation,
            }) => {
                if let Err(mismatch) = accept_start_generation(submitted, task_generation) {
                    let _ = client.request(
                        session_id,
                        runtime_id,
                        wakuwaku_protocol::Command::CloseSession,
                    );
                    return Err(mismatch.into());
                }
                supports_steer
            }
            Ok(_) => anyhow::bail!("WakuWaku daemon returned an invalid start response"),
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
            .name(format!("wakuwaku-daemon-session-{session_id}"))
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
                    if !forwarding_events.send(DriverEvent::RuntimeEventCursorAdvanced(cursor)) {
                        break;
                    }
                    let event = match sequenced.payload {
                        SequencedPayload::Driver { event } => match event_from_wire(event) {
                            Ok(event) => event,
                            Err(error) => DriverEvent::Error(format!(
                                "WakuWaku daemon sent an invalid event: {error}"
                            )),
                        },
                        SequencedPayload::Trajectory { .. } => continue,
                    };
                    saw_process_exit |= matches!(&event, DriverEvent::ProcessExited);
                    if !forwarding_events.send(event) {
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
            inner: Arc::new(DriverHandleInner {
                client,
                session_id,
                runtime_id,
                supports_steer,
                events,
            }),
        })
    }

    fn notify(&self, command: wakuwaku_protocol::Command) {
        if let Err(error) =
            self.inner
                .client
                .notify(self.inner.session_id, self.inner.runtime_id, command)
        {
            let _ = self.inner.events.send(DriverEvent::Error(format!(
                "WakuWaku daemon command failed: {error}"
            )));
        }
    }

    pub fn prompt(&self, input: PromptInput) {
        let client = self.inner.client.clone();
        let session_id = self.inner.session_id;
        let runtime_id = self.inner.runtime_id;
        let events = self.inner.events.clone();
        if let Err(error) = std::thread::Builder::new()
            .name(format!("wakuwaku-prompt-{session_id}"))
            .spawn(move || {
                if let Err(error) = client.request(
                    session_id,
                    runtime_id,
                    wakuwaku_protocol::Command::Prompt { input },
                ) {
                    emit_prompt_failure(&events, error.to_string());
                }
            })
        {
            emit_prompt_failure(&self.inner.events, error.to_string());
        }
    }

    pub fn supports_steer(&self) -> bool {
        self.inner.supports_steer
    }

    pub fn steer(&self, input: PromptInput) {
        self.notify(wakuwaku_protocol::Command::Steer { input });
    }

    pub fn cancel(&self) {
        self.notify(wakuwaku_protocol::Command::Cancel);
    }

    pub fn respond(&self, request_id: String, option_id: String) {
        self.notify(wakuwaku_protocol::Command::Respond {
            request_id,
            option_id,
        });
    }

    pub fn respond_user_input(
        &self,
        request_id: String,
        answers: Vec<wakuwaku_protocol::model::UserInputAnswer>,
    ) {
        self.notify(wakuwaku_protocol::Command::RespondUserInput {
            request_id,
            answers,
        });
    }

    pub fn refresh_background_work(&self) {
        self.notify(wakuwaku_protocol::Command::RefreshBackgroundWork);
    }

    pub fn stop_background_work(
        &self,
        key: wakuwaku_protocol::model::BackgroundWorkKey,
        control_id: String,
    ) {
        self.notify(wakuwaku_protocol::Command::StopBackgroundWork { key, control_id });
    }

    pub fn apply_options(&self, options: SessionOptions) -> bool {
        let options = (|| {
            Ok::<_, anyhow::Error>(wakuwaku_protocol::WireSessionOptions {
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
            self.inner.client.request(
                self.inner.session_id,
                self.inner.runtime_id,
                wakuwaku_protocol::Command::ApplyOptions { options }
            ),
            Ok(wakuwaku_protocol::ResponsePayload::OptionsApplied { applied: true })
        )
    }

    pub fn runtime_id(&self) -> Uuid {
        self.inner.runtime_id
    }

    pub fn session_id(&self) -> Uuid {
        self.inner.session_id
    }

    pub fn close(&self) {
        self.notify(wakuwaku_protocol::Command::CloseSession);
    }
}

impl Drop for DriverHandleInner {
    fn drop(&mut self) {
        self.client.unsubscribe(self.session_id, self.runtime_id);
    }
}

fn emit_prompt_failure(events: &DriverEventSender, message: String) {
    let _ = events.send(DriverEvent::Error(message.clone()));
    let _ = events.send(DriverEvent::TurnFinished {
        success: false,
        summary: Some(message),
    });
}

#[cfg(test)]
mod tests {
    use super::{StartGenerationMismatch, accept_start_generation};

    #[test]
    fn matching_restore_generation_is_accepted() {
        assert_eq!(accept_start_generation(Some(7), Some(7)).unwrap(), Some(7));
    }

    #[test]
    fn start_without_restore_payload_is_accepted() {
        assert_eq!(accept_start_generation(None, Some(3)).unwrap(), Some(3));
        assert_eq!(accept_start_generation(None, None).unwrap(), None);
    }

    #[test]
    fn divergent_restore_generation_fails_closed() {
        assert_eq!(
            accept_start_generation(Some(10), Some(50)).unwrap_err(),
            StartGenerationMismatch {
                submitted: 10,
                accepted: Some(50),
            }
        );
        assert_eq!(
            accept_start_generation(Some(10), None).unwrap_err(),
            StartGenerationMismatch {
                submitted: 10,
                accepted: None,
            }
        );
    }
}
