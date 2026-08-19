//! The small, browser-facing transcript projection.
//!
//! The daemon protocol deliberately carries considerably more information than
//! the first web client needs. Keep the wire decoder in the protocol crate and
//! make this module the one place where the browser decides which events are
//! useful to render. In particular, an event added to the daemon in the
//! future must not make the web client panic.

use wakuwaku_protocol::model::{AgentSession, DriverEvent, MessageRole};
use wakuwaku_protocol::{SequencedEvent, SequencedPayload, event_from_wire};

/// Events understood by the minimal web transcript.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebEvent {
    Connected,
    AutoTitleUpdated(Option<String>),
    TurnStarted,
    TextDelta(String),
    ReasoningDelta(String),
    TurnFinished {
        success: bool,
        summary: Option<String>,
    },
    Error(String),
}

/// Convert a sequenced daemon event into the web projection.
///
/// Trajectory events, malformed payloads, and protocol events that are not part
/// of the minimal transcript are intentionally ignored. This is a projection
/// boundary, so an unknown event is not a fatal transport error.
pub fn reduce_event(event: &SequencedEvent) -> Option<WebEvent> {
    let SequencedPayload::Driver { event } = &event.payload else {
        return None;
    };

    let event = event_from_wire(event.clone()).ok()?;
    match event {
        DriverEvent::Connected => Some(WebEvent::Connected),
        DriverEvent::AutoTitleUpdated(title) => Some(WebEvent::AutoTitleUpdated(title)),
        DriverEvent::TurnStarted => Some(WebEvent::TurnStarted),
        DriverEvent::TextDelta(text) => Some(WebEvent::TextDelta(text)),
        DriverEvent::ReasoningDelta(text) => Some(WebEvent::ReasoningDelta(text)),
        DriverEvent::TurnFinished { success, summary } => {
            Some(WebEvent::TurnFinished { success, summary })
        }
        DriverEvent::Error(error) => Some(WebEvent::Error(error)),
        _ => None,
    }
}

/// A transcript row understood by the web renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebMessage {
    pub role: MessageRole,
    pub content: String,
}

impl WebMessage {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// State needed to render one selected web session.
///
/// This is intentionally smaller than [`AgentSession`]. The daemon remains
/// authoritative for persistence and provider state; the browser only keeps
/// the title, visible messages, live reasoning, and the current turn result.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TranscriptState {
    pub connected: bool,
    pub title: Option<String>,
    pub messages: Vec<WebMessage>,
    pub reasoning: String,
    pub turn_in_progress: bool,
    pub last_result: Option<TurnResult>,
    pub error: Option<String>,
}

/// Result of the most recently completed turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnResult {
    pub success: bool,
    pub summary: Option<String>,
}

impl TranscriptState {
    /// Replace the projection with a hydrated daemon session.
    pub fn hydrate(&mut self, session: &AgentSession) {
        self.connected = true;
        self.title = Some(session.display_title().to_owned());
        self.messages = session
            .messages
            .iter()
            .map(|message| WebMessage::new(message.role, message.visible_content()))
            .collect();
        self.reasoning.clear();
        self.turn_in_progress = session.status.is_busy();
        self.last_result = None;
        self.error = None;
    }

    /// Apply one already-reduced event to the projection.
    pub fn apply(&mut self, event: WebEvent) {
        match event {
            WebEvent::Connected => {
                self.connected = true;
                self.error = None;
            }
            WebEvent::AutoTitleUpdated(title) => {
                self.title = title;
            }
            WebEvent::TurnStarted => {
                self.turn_in_progress = true;
                self.reasoning.clear();
                self.last_result = None;
                self.error = None;
                self.ensure_assistant_message();
            }
            WebEvent::TextDelta(delta) => {
                self.ensure_assistant_message();
                if let Some(message) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.role == MessageRole::Assistant)
                {
                    message.content.push_str(&delta);
                }
            }
            WebEvent::ReasoningDelta(delta) => {
                self.turn_in_progress = true;
                self.reasoning.push_str(&delta);
            }
            WebEvent::TurnFinished { success, summary } => {
                self.turn_in_progress = false;
                self.last_result = Some(TurnResult { success, summary });
            }
            WebEvent::Error(error) => {
                self.turn_in_progress = false;
                self.error = Some(error);
            }
        }
    }

    fn ensure_assistant_message(&mut self) {
        let needs_message = !matches!(
            self.messages.last(),
            Some(WebMessage {
                role: MessageRole::Assistant,
                ..
            })
        );
        if needs_message {
            self.messages
                .push(WebMessage::new(MessageRole::Assistant, ""));
        }
    }
}
