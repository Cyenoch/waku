//! Daemon-owned in-process provider runtime.

mod activity;
mod embedded;

/// Exact system prompt installed on every fresh embedded session.
///
/// Existing snapshots keep whatever prompt they already stored. Mode changes
/// must not rewrite this text.
pub const WAKU_SYSTEM_PROMPT: &str = "You are WakuWaku, a coding assistant working in the user's workspace.

Use the available tools to inspect and change files. Prefer precise, minimal edits. Do not invent file contents or command results you have not observed.

Interaction modes:
- Build: implement the requested change. You may edit files and, when permitted, run shell commands.
- Plan: analyze the request and propose an approach. Do not edit files or run mutating shell commands; use read-only inspection only.

Follow the user's instructions. Ask when a required choice is ambiguous.";

use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::model::{DriverEvent, InteractionMode, ProviderId, RuntimeMode};
use uuid::Uuid;
use wakuwaku_protocol::ExternalProvider;

/// Provider events remain synchronous to send from reader threads, while the
/// bounded wake channel lets the UI sleep until at least one event is ready.
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

#[cfg(test)]
pub(crate) fn test_event_channel() -> (DriverEventSender, Receiver<DriverEvent>) {
    let (wake, _wakes) = smol::channel::bounded(1);
    event_channel(wake)
}

#[derive(Clone)]
pub struct DriverHandle {
    inner: Arc<embedded::EmbeddedDriver>,
}

impl DriverHandle {
    pub fn prompt(&self, message: wakuwaku_harness::UserMessage) {
        self.inner.prompt(message);
    }

    pub fn supports_steer(&self) -> bool {
        self.inner.supports_steer()
    }

    pub fn steer(&self, message: wakuwaku_harness::UserMessage) {
        self.inner.steer(message);
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn respond(&self, request_id: String, option_id: String) {
        self.inner.respond(request_id, option_id);
    }

    pub fn respond_user_input(
        &self,
        request_id: String,
        answers: Vec<wakuwaku_protocol::model::UserInputAnswer>,
    ) -> anyhow::Result<()> {
        self.inner.respond_user_input(request_id, answers)
    }

    pub fn reject_background_stop(&self, key: wakuwaku_protocol::model::BackgroundWorkKey) {
        self.inner.reject_background_stop(key);
    }

    pub fn apply_options(&self, options: SessionOptions) -> bool {
        self.inner.apply_options(options)
    }

    pub fn replace_auth(
        &self,
        auth: wakuwaku_harness::Auth,
        extra_auth_headers: Vec<(String, String)>,
    ) -> anyhow::Result<()> {
        self.inner.replace_auth(auth, extra_auth_headers)
    }

    pub fn snapshot(&self) -> anyhow::Result<wakuwaku_harness::SessionSnapshot> {
        self.inner.snapshot()
    }
}

pub struct DriverStartOptions {
    pub provider: ExternalProvider,
    pub cwd: PathBuf,
    pub mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<wakuwaku_protocol::ServiceTier>,
    pub context_window: Option<String>,
    pub auth: wakuwaku_harness::Auth,
    pub transport: wakuwaku_protocol::TransportProfile,
    pub extra_auth_headers: Vec<(String, String)>,
    pub capabilities: wakuwaku_protocol::ModelCapabilities,
    pub limits: wakuwaku_protocol::ProviderLimits,
    pub(crate) snapshot: wakuwaku_harness::SessionSnapshot,
}

#[derive(Clone, Debug)]
pub struct SessionReconfigure {
    pub provider: ExternalProvider,
    pub auth: wakuwaku_harness::Auth,
    pub transport: wakuwaku_protocol::TransportProfile,
    pub extra_auth_headers: Vec<(String, String)>,
    pub capabilities: wakuwaku_protocol::ModelCapabilities,
    pub limits: wakuwaku_protocol::ProviderLimits,
}

#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<wakuwaku_protocol::ServiceTier>,
    pub context_window: Option<String>,
    pub reconfigure: Option<SessionReconfigure>,
}

pub(crate) fn start_local(
    provider: ProviderId,
    options: DriverStartOptions,
    events: DriverEventSender,
    session_id: Uuid,
    handoff: std::sync::Arc<crate::trajectory::TraceHandoff>,
) -> anyhow::Result<DriverHandle> {
    Ok(DriverHandle {
        inner: Arc::new(embedded::EmbeddedDriver::start(
            provider, options, events, session_id, handoff,
        )?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InteractionMode, RuntimeMode};
    use std::time::{Duration, Instant};
    use wakuwaku_protocol::{ModelCapabilities, ProviderPreset};

    fn start_options(preset: ProviderPreset) -> DriverStartOptions {
        let format = preset.default_format();
        DriverStartOptions {
            provider: preset.endpoint(),
            cwd: std::env::temp_dir(),
            mode: RuntimeMode::Ask,
            interaction_mode: InteractionMode::Build,
            model: Some(preset.default_model().to_owned()),
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
            limits: Default::default(),
            auth: wakuwaku_harness::Auth::Bearer("test".into()),
            transport: preset.transport(),
            extra_auth_headers: Vec::new(),
            capabilities: match preset {
                ProviderPreset::OpenAiCodex => ModelCapabilities::codex(),
                ProviderPreset::Anthropic => ModelCapabilities::anthropic(),
                ProviderPreset::Xai | ProviderPreset::XaiOauth => ModelCapabilities::xai(false),
                _ => ModelCapabilities::openai_compatible(format),
            },
            snapshot: wakuwaku_harness::Session::new(None).snapshot(),
        }
    }

    #[test]
    fn provider_events_coalesce_wakes_without_dropping_payloads() {
        let (wake, wakes) = smol::channel::bounded(1);
        let (events, received) = event_channel(wake);

        assert!(events.send(DriverEvent::TextDelta("one".into())));
        assert!(events.send(DriverEvent::TextDelta("two".into())));

        assert_eq!(wakes.try_recv(), Ok(()));
        assert!(matches!(
            wakes.try_recv(),
            Err(smol::channel::TryRecvError::Empty)
        ));
        assert!(matches!(received.try_recv(), Ok(DriverEvent::TextDelta(text)) if text == "one"));
        assert!(matches!(received.try_recv(), Ok(DriverEvent::TextDelta(text)) if text == "two"));
    }

    #[test]
    fn every_preset_constructs_an_embedded_driver_never_a_provider_binary() {
        for preset in ProviderPreset::ALL {
            let (events, received) = test_event_channel();
            let handle = start_local(
                preset.provider_id(),
                start_options(preset),
                events,
                Uuid::nil(),
                Arc::new(crate::trajectory::TraceHandoff::new()),
            )
            .unwrap_or_else(|error| {
                let rendered = error.to_string();
                assert!(
                    !rendered.contains("installed")
                        && !rendered.contains("尚未安装")
                        && !rendered.contains("could not be found")
                        && !rendered.contains("not found"),
                    "{} used a binary/install path: {rendered}",
                    preset.id()
                );
                panic!(
                    "{} must construct the embedded driver: {rendered}",
                    preset.id()
                );
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match received.recv_timeout(remaining) {
                    Ok(DriverEvent::Connected) => break,
                    Ok(DriverEvent::Error(error)) => {
                        assert!(
                            !error.contains("installed")
                                && !error.contains("尚未安装")
                                && !error.contains("could not be found"),
                            "{} used a binary/install path: {error}",
                            preset.id()
                        );
                        panic!("{} failed after start: {error}", preset.id());
                    }
                    Ok(_) => {}
                    Err(_) => panic!("{} never emitted Connected", preset.id()),
                }
            }
            drop(handle);
        }
    }
}
