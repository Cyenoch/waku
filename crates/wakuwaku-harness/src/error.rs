//! Structured harness errors.
//!
//! Adapter implementations surface failures as [`HarnessError`] internally;
//! the public stream contract converts them into terminal `Error` events so
//! stream construction itself never fails. API keys are never formatted into
//! error output.

use thiserror::Error;

/// Top-level error type crossing crate boundaries.
#[derive(Debug, Error)]
pub enum HarnessError {
    /// The referenced provider id is not registered.
    #[error("unknown provider: {0}")]
    UnknownProvider(String),

    /// The provider is registered but has no usable auth configuration.
    #[error("provider {provider} is not configured (missing auth)")]
    NotConfigured { provider: String },

    /// A provider endpoint returned a non-2xx response.
    #[error("provider {provider} returned HTTP {status}: {body}")]
    Http {
        provider: String,
        status: u16,
        body: String,
        /// Server-requested retry delay parsed from `retry-after` headers.
        retry_after: Option<std::time::Duration>,
    },

    /// The HTTP layer failed before a response arrived (connect, TLS, …).
    ///
    /// The underlying client error is intentionally not retained: reqwest's
    /// display/debug output includes the request URL, which could contain a
    /// credential in a configured endpoint.
    #[error("transport error")]
    Transport,

    /// The SSE stream ended without a terminal protocol event.
    #[error("stream ended without a terminal event ({format})")]
    MissingTerminal { format: &'static str },

    /// A payload could not be decoded (bad JSON, malformed SSE data, …).
    #[error("malformed {format} payload: {detail}")]
    Malformed {
        format: &'static str,
        detail: String,
    },

    /// A terminal event reported an invalid state (e.g. pending stop reason).
    #[error("invalid stream terminal state ({format}): {detail}")]
    InvalidTerminal {
        format: &'static str,
        detail: String,
    },

    /// The run was cancelled through its cancel token.
    #[error("run cancelled")]
    Cancelled,

    /// The conversation no longer fits the configured message budget.
    #[error("context overflow: {needed} messages/messages-equivalent exceed budget {budget}")]
    ContextOverflow { needed: u64, budget: u64 },

    /// Request construction failed against the harness invariants.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl HarnessError {
    /// Whether a transient-failure retry should be attempted for this error.
    pub fn is_retryable_transport(&self) -> bool {
        match self {
            HarnessError::Http { status, .. } => {
                matches!(*status, 408 | 409 | 429) || *status >= 500
            }
            HarnessError::Transport => true,
            _ => false,
        }
    }

    /// Server-requested retry delay, when present.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            HarnessError::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}
