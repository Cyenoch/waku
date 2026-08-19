//! HTTP provider execution: retry, SSE driving, and the model dyn seam.
//!
//! `ModelProvider` is one of the crate's two dyn seams (the other is `Tool`).
//! `HttpProvider` implements it against the provider registry; scripted
//! implementations drive the agent-loop tests.

use crate::adapter::{AssistantScratch, PayloadOutcome, anthropic, chat, responses};
use crate::agent::estimate_tokens;
use crate::cancel::CancelToken;
use crate::error::HarnessError;
use crate::events::StreamEvent;
use crate::model::{
    ApiFormat, AssistantMessage, PromptContext, ProviderModel, RequestOptions, StopReason,
};
use crate::provider::{ProviderConfig, ProviderRequest, Providers};
use futures::StreamExt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Retry policy for transport-level failures.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    /// Cap on server-requested delays; longer requests fail immediately.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_retry_after: Duration::from_secs(60),
        }
    }
}

/// The model-provider dyn seam: one method; everything else is configuration.
pub trait ModelProvider: Send + Sync {
    /// Stream one assistant response. Events flow to `sink`; the finalized
    /// message returns by value. Failures return `Err` with a structured
    /// error; cancellation returns `Err(Cancelled)`.
    fn complete<'a>(
        &'a self,
        ctx: &'a PromptContext,
        opts: &'a RequestOptions,
        model: Option<&'a str>,
        cancel: CancelToken,
        sink: &'a mut (dyn FnMut(StreamEvent) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<AssistantMessage, HarnessError>> + Send + 'a>>;
}

/// HTTP+SSE provider executing against a configured endpoint registry.
pub struct HttpProvider {
    providers: Providers,
    provider_id: String,
    retry: RetryPolicy,
}

impl HttpProvider {
    pub fn new(providers: Providers, provider_id: impl Into<String>) -> Result<Self, HarnessError> {
        let provider_id = provider_id.into();
        if providers.get(&provider_id).is_none() {
            return Err(HarnessError::UnknownProvider(provider_id));
        }
        Ok(HttpProvider {
            providers,
            provider_id,
            retry: RetryPolicy::default(),
        })
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn replace_auth(
        &self,
        auth: crate::provider::Auth,
        extra_auth_headers: crate::provider::ExtraHeaders,
    ) -> Result<(), HarnessError> {
        self.providers
            .replace_auth(&self.provider_id, auth, extra_auth_headers)
    }

    pub fn replace_config(
        &self,
        config: crate::provider::ProviderConfig,
    ) -> Result<(), HarnessError> {
        self.providers.set_providers(vec![config])
    }
}

impl ModelProvider for HttpProvider {
    fn complete<'a>(
        &'a self,
        ctx: &'a PromptContext,
        opts: &'a RequestOptions,
        model: Option<&'a str>,
        cancel: CancelToken,
        sink: &'a mut (dyn FnMut(StreamEvent) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<AssistantMessage, HarnessError>> + Send + 'a>> {
        Box::pin(async move {
            let (config, model_id) = self.providers.resolve_model(&self.provider_id, model)?;
            let needed = estimate_tokens(&ctx.messages);
            if needed > config.limits.context_window {
                return Err(HarnessError::ContextOverflow {
                    needed,
                    budget: config.limits.context_window,
                });
            }
            let mut effective_options = opts.clone();
            effective_options.max_tokens = Some(
                opts.max_tokens
                    .unwrap_or(config.limits.max_output_tokens)
                    .min(config.limits.max_output_tokens),
            );
            let target = ProviderModel {
                provider: wakuwaku_provider::ProviderId::new(self.provider_id.as_str()),
                model: model_id.clone(),
            };
            let body = crate::adapter::build_body(
                config.endpoint.api_format,
                ctx,
                &target,
                &effective_options,
            )?;
            let req = self.providers.build_request(&config, body)?;

            let mut attempt: u32 = 0;
            loop {
                attempt += 1;
                cancel.check()?;
                match execute_once(&self.providers, &config, &req, &model_id, &cancel, sink).await {
                    Ok(msg) => return Ok(msg),
                    Err(err) => {
                        let retryable = err.is_retryable_transport()
                            && attempt < self.retry.max_attempts
                            && !cancel.is_cancelled();
                        if !retryable {
                            return Err(err);
                        }
                        let delay = match err.retry_after() {
                            Some(d) if d > self.retry.max_retry_after => return Err(err),
                            Some(d) => d,
                            None => backoff(self.retry.base_delay, attempt),
                        };
                        crate::cancel::backoff_sleep(&cancel, delay).await?;
                    }
                }
            }
        })
    }
}

fn backoff(base: Duration, attempt: u32) -> Duration {
    let shift = (attempt.saturating_sub(1)).min(5);
    base.saturating_mul(1u32 << shift)
}

#[derive(Debug)]
enum Attempt {
    Failed(HarnessError),
    Settled(AssistantMessage),
}

async fn execute_once(
    providers: &Providers,
    config: &ProviderConfig,
    req: &ProviderRequest,
    model_id: &str,
    cancel: &CancelToken,
    sink: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<AssistantMessage, HarnessError> {
    let http = providers.http();
    let request = http
        .request(req.method.clone(), &req.url)
        .headers(req.headers.clone())
        .timeout(Duration::from_secs(600))
        .json(&req.body);
    let send = request.send();
    futures::pin_mut!(send);
    let response = match futures::future::select(send, cancel.cancelled()).await {
        futures::future::Either::Left((res, _)) => res.map_err(|_| HarnessError::Transport)?,
        futures::future::Either::Right(_) => return Err(HarnessError::Cancelled),
    };

    let status = response.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(response.headers());
        let body_future = response.text();
        futures::pin_mut!(body_future);
        let body = match futures::future::select(body_future, cancel.cancelled()).await {
            futures::future::Either::Left((body, _)) => body.unwrap_or_default(),
            futures::future::Either::Right(_) => return Err(HarnessError::Cancelled),
        };
        let body = truncate(&redact_provider_secrets(&body, config), 2000);
        return Err(HarnessError::Http {
            provider: config.endpoint.id.as_str().to_owned(),
            status: status.as_u16(),
            body,
            retry_after,
        });
    }
    sink(StreamEvent::Start);
    let mut scratch = AssistantScratch::new(model_id, config.endpoint.id.as_str());
    let outcome = match config.endpoint.api_format {
        ApiFormat::OpenAiResponses => {
            let mut slots = responses::Slots::new();
            run_sse(
                SseDrive {
                    response,
                    format: responses::FORMAT,
                    scratch: &mut scratch,
                    cancel,
                    sink,
                    state: &mut (),
                },
                |payload, scratch, _, sink| {
                    responses::process_payload(payload, scratch, &mut slots)
                        .map(|o| deliver(o, sink))
                },
                |_scratch, _, _sink| Ok(None),
            )
            .await
        }
        ApiFormat::OpenAiChat => {
            let mut state = chat::ChatState::new();
            run_sse(
                SseDrive {
                    response,
                    format: chat::FORMAT,
                    scratch: &mut scratch,
                    cancel,
                    sink,
                    state: &mut state,
                },
                |payload, scratch, state, sink| {
                    chat::process_payload(payload, scratch, state).map(|o| deliver(o, sink))
                },
                |scratch, state, sink| {
                    chat::finish_pending(scratch, state, Vec::new()).map(|o| deliver(o, sink))
                },
            )
            .await
        }
        ApiFormat::Anthropic => {
            let mut state = anthropic::AnthropicState::new();
            run_sse(
                SseDrive {
                    response,
                    format: anthropic::FORMAT,
                    scratch: &mut scratch,
                    cancel,
                    sink,
                    state: &mut state,
                },
                |payload, scratch, state, sink| {
                    anthropic::process_payload(payload, scratch, state).map(|o| deliver(o, sink))
                },
                |_scratch, _, _sink| Ok(None),
            )
            .await
        }
    };
    match outcome {
        Attempt::Settled(msg) => Ok(msg),
        Attempt::Failed(err) => {
            let taken = take_scratch(&mut scratch);
            let (msg, ev) = taken.fail(err);
            sink(ev);
            Ok(*msg)
        }
    }
}

fn deliver(
    outcome: PayloadOutcome,
    sink: &mut (dyn FnMut(StreamEvent) + Send),
) -> Option<AssistantMessage> {
    match outcome {
        PayloadOutcome::Events(events) => {
            for ev in events {
                sink(ev);
            }
            None
        }
        PayloadOutcome::Terminal(msg, events) => {
            for event in events {
                sink(event);
            }
            Some(*msg)
        }
    }
}

/// Drive the SSE body to a terminal payload, honoring `[DONE]`, EOF-flush,
/// and missing-terminal conditions. `on_end` lets Chat Completions defer its
/// finish_reason until the usage chunk, `[DONE]`, or EOF.
struct SseDrive<'a, S> {
    response: reqwest::Response,
    format: &'static str,
    scratch: &'a mut AssistantScratch,
    cancel: &'a CancelToken,
    sink: &'a mut (dyn FnMut(StreamEvent) + Send),
    state: &'a mut S,
}

async fn run_sse<S, F, E>(drive: SseDrive<'_, S>, mut on_payload: F, mut on_end: E) -> Attempt
where
    F: FnMut(
            &str,
            &mut AssistantScratch,
            &mut S,
            &mut (dyn FnMut(StreamEvent) + Send),
        ) -> Result<Option<AssistantMessage>, HarnessError>
        + Send,
    E: FnMut(
            &mut AssistantScratch,
            &mut S,
            &mut (dyn FnMut(StreamEvent) + Send),
        ) -> Result<Option<AssistantMessage>, HarnessError>
        + Send,
{
    let SseDrive {
        response,
        format,
        scratch,
        cancel,
        sink,
        state,
    } = drive;
    let byte_stream = response.bytes_stream();
    let mut events = Box::pin(crate::sse::sse_stream(byte_stream, format));
    let mut saw_data = false;

    loop {
        let next = events.next();
        futures::pin_mut!(next);
        let item = match futures::future::select(next, cancel.cancelled()).await {
            futures::future::Either::Left((item, _)) => item,
            futures::future::Either::Right(_) => {
                let taken = take_scratch(scratch);
                let (msg, ev) = taken.fail(HarnessError::Cancelled);
                sink(ev);
                return Attempt::Settled(*msg);
            }
        };
        let Some(item) = item else { break };
        let evt = match item {
            Ok(e) => e,
            Err(e) => return Attempt::Failed(e),
        };
        saw_data = true;
        if evt.data.trim() == "[DONE]" {
            break;
        }
        match on_payload(&evt.data, scratch, state, sink) {
            Ok(Some(msg)) => return Attempt::Settled(msg),
            Ok(None) => {}
            Err(e) => return Attempt::Failed(e),
        }
    }

    match on_end(scratch, state, sink) {
        Ok(Some(msg)) => return Attempt::Settled(msg),
        Ok(None) => {}
        Err(e) => return Attempt::Failed(e),
    }

    if scratch.msg.stop_reason != StopReason::Pending {
        let taken = take_scratch(scratch);
        let reason = taken.msg.stop_reason;
        let error = taken.msg.error_message.clone();
        let (msg, ev) = taken.finish(reason, error);
        sink(ev);
        return Attempt::Settled(*msg);
    }

    if !saw_data {
        return Attempt::Failed(HarnessError::Malformed {
            format,
            detail: "empty SSE body".into(),
        });
    }
    Attempt::Failed(HarnessError::MissingTerminal { format })
}

fn take_scratch(scratch: &mut AssistantScratch) -> AssistantScratch {
    let model = scratch.msg.model.clone();
    let provider = scratch.msg.provider.clone();
    std::mem::replace(scratch, AssistantScratch::new(&model, &provider))
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(v) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok())
        && let Ok(ms) = v.trim().parse::<u64>()
    {
        return Some(Duration::from_millis(ms));
    }
    let v = headers.get("retry-after")?.to_str().ok()?;
    if let Ok(secs) = v.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    None
}

fn redact_provider_secrets(body: &str, config: &ProviderConfig) -> String {
    let mut redacted = body.to_string();
    for secret in auth_secrets(config) {
        if !secret.is_empty() {
            redacted = redacted.replace(secret, "[redacted]");
        }
    }
    if !config.endpoint.base_url.is_empty() {
        redacted = redacted.replace(&config.endpoint.base_url, "[redacted-endpoint]");
    }
    redacted
}

fn auth_secrets(config: &ProviderConfig) -> Vec<&str> {
    let mut secrets: Vec<&str> = Vec::with_capacity(config.endpoint.headers.len() + 1);
    match &config.auth {
        crate::provider::Auth::Bearer(secret)
        | crate::provider::Auth::AnthropicApiKey { key: secret, .. } => {
            secrets.push(secret.as_str())
        }
        crate::provider::Auth::None => {}
    }
    secrets.extend(
        config
            .endpoint
            .headers
            .iter()
            .map(|(_, value)| value.as_str()),
    );
    secrets
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Shared, cloneable provider handle.
pub type SharedProvider = Arc<dyn ModelProvider>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ApiFormat;
    use crate::provider::{Auth, ProviderConfig};

    #[test]
    fn redact_provider_secrets_strips_auth_and_header_values() {
        let config = ProviderConfig {
            endpoint: wakuwaku_provider::ExternalProvider {
                id: wakuwaku_provider::ProviderId::new("p"),
                name: "P".into(),
                base_url: "https://gateway.example/v1".into(),
                api_format: ApiFormat::OpenAiChat,
                headers: vec![(
                    "x-gateway-token".into(),
                    "gateway-token-should-never-leak".into(),
                )],
            },
            limits: wakuwaku_provider::ProviderLimits {
                context_window: 1000,
                max_output_tokens: 100,
            },
            auth: Auth::Bearer("auth-secret".into()),
            transport: wakuwaku_provider::TransportProfile::Standard,
            extra_auth_headers: Vec::new(),
        };
        let body = redact_provider_secrets(
            "upstream echoed gateway-token-should-never-leak and auth-secret at https://gateway.example/v1",
            &config,
        );
        assert!(!body.contains("gateway-token-should-never-leak"));
        assert!(!body.contains("auth-secret"));
        assert!(!body.contains("https://gateway.example/v1"));
        assert!(body.contains("[redacted]"));
    }
}
