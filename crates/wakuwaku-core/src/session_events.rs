//! Shadow session event log: a strict, diagnostics-only record of the events
//! this daemon actually produces. Not a canonical replay source; capture is
//! opt-in and never delays live delivery.

use uuid::Uuid;

/// Payload schema of every row written in this tranche.
pub const SESSION_EVENT_SCHEMA_VERSION: i64 = 1;

/// Cap for persisted free-text metadata (display text, titles, summaries,
/// errors, reject reasons), counted in Unicode scalar values.
const BOUNDED_TEXT_SCALARS: usize = 240;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEventPayload {
    PromptObserved {
        digest: String,
        display_text: Option<String>,
        attachment_count: usize,
    },
    TurnStarted {},
    UsageRecorded {
        usage_event_id: Uuid,
        provider: String,
        model: String,
        timestamp_ms: i64,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: Option<u64>,
        context_tokens: Option<u64>,
        context_window: Option<u64>,
    },
    TurnFinished {
        success: bool,
        summary: Option<String>,
    },
    PermissionRequested {
        request_id: String,
        title: String,
    },
    UserInputRequested {
        request_id: String,
        question_count: usize,
    },
    SteerAccepted {
        digest: String,
    },
    SteerRejected {
        digest: String,
        reason: String,
    },
    BackgroundWork {
        work_kind: crate::model::BackgroundWorkKind,
        provider_id: String,
        status: crate::model::BackgroundWorkStatus,
    },
    Error {
        message: String,
    },
    ProcessExited {},
}

impl SessionEventPayload {
    /// The exact serde tag stored in the `kind` column.
    pub fn kind(&self) -> &'static str {
        match self {
            SessionEventPayload::PromptObserved { .. } => "prompt_observed",
            SessionEventPayload::TurnStarted {} => "turn_started",
            SessionEventPayload::UsageRecorded { .. } => "usage_recorded",
            SessionEventPayload::TurnFinished { .. } => "turn_finished",
            SessionEventPayload::PermissionRequested { .. } => "permission_requested",
            SessionEventPayload::UserInputRequested { .. } => "user_input_requested",
            SessionEventPayload::SteerAccepted { .. } => "steer_accepted",
            SessionEventPayload::SteerRejected { .. } => "steer_rejected",
            SessionEventPayload::BackgroundWork { .. } => "background_work",
            SessionEventPayload::Error { .. } => "error",
            SessionEventPayload::ProcessExited {} => "process_exited",
        }
    }

    /// Strict decode: rejects unknown schema versions, unknown fields,
    /// malformed JSON, and a `kind` column that disagrees with the payload.
    pub fn decode(
        schema_version: i64,
        kind: &str,
        payload_json: &str,
    ) -> Result<Self, SessionEventError> {
        if schema_version != SESSION_EVENT_SCHEMA_VERSION {
            return Err(SessionEventError::Schema {
                found: schema_version,
                expected: SESSION_EVENT_SCHEMA_VERSION,
            });
        }
        let payload: SessionEventPayload = serde_json::from_str(payload_json)
            .map_err(|error| SessionEventError::Payload(error.to_string()))?;
        let decoded = payload.kind();
        if decoded != kind {
            return Err(SessionEventError::KindMismatch {
                column: kind.to_owned(),
                payload: decoded.to_owned(),
            });
        }
        Ok(payload)
    }
}

#[derive(Clone, Debug)]
pub struct NewSessionEvent {
    pub event_id: Uuid,
    pub command_id: Option<String>,
    pub created_at_ms: i64,
    pub runtime_id: Option<Uuid>,
    pub turn_id: Option<Uuid>,
    pub payload: SessionEventPayload,
}

impl NewSessionEvent {
    pub fn observed(
        runtime_id: Option<Uuid>,
        turn_id: Option<Uuid>,
        payload: SessionEventPayload,
    ) -> Self {
        Self {
            event_id: Uuid::new_v4(),
            command_id: None,
            created_at_ms: i64::try_from(crate::model::unix_time_millis()).unwrap_or(i64::MAX),
            runtime_id,
            turn_id,
            payload,
        }
    }

    pub fn with_command_id(mut self, command_id: String) -> Self {
        self.command_id = Some(command_id);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionEventError {
    #[error("unsupported session event schema {found}; expected {expected}")]
    Schema { found: i64, expected: i64 },
    #[error("session event payload is invalid: {0}")]
    Payload(String),
    #[error("session event kind column {column:?} does not match payload kind {payload:?}")]
    KindMismatch { column: String, payload: String },
}

/// SHA-256 over a prompt/steer text, stored instead of the raw text.
pub fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Truncates persisted free text to a bounded number of scalar values.
pub fn bounded_text(value: String) -> String {
    value.chars().take(BOUNDED_TEXT_SCALARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_matches_serde_tag_for_every_variant() {
        for (payload, kind) in [
            (
                SessionEventPayload::PromptObserved {
                    digest: "d".into(),
                    display_text: None,
                    attachment_count: 0,
                },
                "prompt_observed",
            ),
            (SessionEventPayload::TurnStarted {}, "turn_started"),
            (
                SessionEventPayload::UsageRecorded {
                    usage_event_id: Uuid::nil(),
                    provider: "p".into(),
                    model: "m".into(),
                    timestamp_ms: 1,
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: None,
                    context_tokens: None,
                    context_window: None,
                },
                "usage_recorded",
            ),
            (
                SessionEventPayload::TurnFinished {
                    success: true,
                    summary: None,
                },
                "turn_finished",
            ),
            (
                SessionEventPayload::PermissionRequested {
                    request_id: "r".into(),
                    title: "t".into(),
                },
                "permission_requested",
            ),
            (
                SessionEventPayload::UserInputRequested {
                    request_id: "r".into(),
                    question_count: 2,
                },
                "user_input_requested",
            ),
            (
                SessionEventPayload::SteerAccepted { digest: "d".into() },
                "steer_accepted",
            ),
            (
                SessionEventPayload::SteerRejected {
                    digest: "d".into(),
                    reason: "busy".into(),
                },
                "steer_rejected",
            ),
            (
                SessionEventPayload::BackgroundWork {
                    work_kind: crate::model::BackgroundWorkKind::Process,
                    provider_id: "p".into(),
                    status: crate::model::BackgroundWorkStatus::Running,
                },
                "background_work",
            ),
            (
                SessionEventPayload::Error {
                    message: "e".into(),
                },
                "error",
            ),
            (SessionEventPayload::ProcessExited {}, "process_exited"),
        ] {
            assert_eq!(payload.kind(), kind);
            let json = serde_json::to_string(&payload).unwrap();
            assert!(json.contains(&format!(r#""kind":"{kind}""#)), "{json}");
            assert_eq!(
                SessionEventPayload::decode(SESSION_EVENT_SCHEMA_VERSION, kind, &json).unwrap(),
                payload
            );
        }
    }

    #[test]
    fn decode_rejects_unknown_schema_fields_and_kind_mismatch() {
        let json = serde_json::to_string(&SessionEventPayload::TurnStarted {}).unwrap();
        assert!(matches!(
            SessionEventPayload::decode(2, "turn_started", &json),
            Err(SessionEventError::Schema { found: 2, .. })
        ));
        assert!(SessionEventPayload::decode(1, "turn_finished", &json).is_err());
        assert!(SessionEventPayload::decode(1, "turn_started", "not json").is_err());
        let extra = r#"{"kind":"prompt_observed","digest":"d","unexpected":1}"#;
        assert!(
            SessionEventPayload::decode(1, "prompt_observed", extra).is_err(),
            "struct variants must deny unknown fields"
        );
        let unit_extra = r#"{"kind":"turn_started","unexpected":1}"#;
        assert!(
            SessionEventPayload::decode(1, "turn_started", unit_extra).is_err(),
            "unit variants must reject residual keys"
        );
    }

    #[test]
    fn bounded_text_truncates_on_scalar_boundaries() {
        let text: String = "α".repeat(300);
        let bounded = bounded_text(text);
        assert_eq!(bounded.chars().count(), 240);
        assert_eq!(bounded_text("short".into()), "short");
    }

    #[test]
    fn sha256_hex_matches_known_digest() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
