use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use waku_harness::{AgentEvent, StreamEvent};
use waku_harness::{
    ApiFormat, AssistantMessage, ContentBlock, Message, PromptContext, RequestOptions, StopReason,
    TextBlock, ThinkingBlock, ToolCall, ToolSchema, Usage, UserMessage,
};
use waku_harness::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, ApprovalTool, ExecOutcome, ExecutionContext,
    ExecutionMode, Tool, ToolContext, ToolError, ToolSpec,
};
use waku_harness::{
    Auth, Budget, CancelToken, HarnessError, HttpProvider, ModelProvider, ProviderConfig,
    Providers, QueueMode, Session, SessionSteering,
};
use waku_harness::{Harness, RunOutcome};

#[derive(Debug, Clone)]
struct ResponseSpec {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    delay: Option<Duration>,
}

impl ResponseSpec {
    fn ok(body: impl Into<String>) -> Self {
        ResponseSpec {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.into(),
            delay: None,
        }
    }

    fn error(status: u16, headers: Vec<(String, String)>, body: impl Into<String>) -> Self {
        ResponseSpec {
            status,
            headers,
            body: body.into(),
            delay: None,
        }
    }
}

#[derive(Debug, Clone)]
struct RequestRecord {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<RequestRecord>>>,
    task: JoinHandle<()>,
}

impl MockServer {
    fn requests(&self) -> Vec<RequestRecord> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }
}

async fn spawn_mock(specs: Vec<ResponseSpec>) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        for spec in specs {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await.unwrap();
            if let Ok(mut records) = records.lock() {
                records.push(request);
            }
            if let Some(delay) = spec.delay {
                sleep(delay).await;
            }
            write_response(&mut socket, &spec).await.unwrap();
        }
    });
    MockServer {
        base_url: format!("http://{address}"),
        requests,
        task,
    }
}

async fn read_request(
    socket: &mut TcpStream,
) -> Result<RequestRecord, Box<dyn std::error::Error + Send + Sync>> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 4096];
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Err("request ended before headers".into());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
    };
    let header_text = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines.next().ok_or("missing request line")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().ok_or("missing method")?.to_string();
    let path = request_parts.next().ok_or("missing path")?.to_string();
    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse()?;
            }
            headers.insert(name, value);
        }
    }
    let body_start = header_end + 4;
    while bytes.len() < body_start + content_length {
        let mut chunk = [0u8; 4096];
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Err("request ended before body".into());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&bytes[body_start..body_start + content_length])?;
    Ok(RequestRecord {
        method,
        path,
        headers,
        body,
    })
}

async fn write_response(
    socket: &mut TcpStream,
    spec: &ResponseSpec,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let reason = match spec.status {
        200 => "OK",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let body = spec.body.as_bytes();
    let mut response = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        spec.status,
        reason,
        body.len()
    );
    for (name, value) in &spec.headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    socket.write_all(response.as_bytes()).await?;
    socket.write_all(body).await?;
    socket.shutdown().await?;
    Ok(())
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect()
}

fn provider_config(base_url: &str, format: ApiFormat, auth: Auth) -> ProviderConfig {
    ProviderConfig {
        endpoint: waku_provider::ExternalProvider {
            id: waku_provider::ProviderId::new("test-provider"),
            name: "Test Provider".into(),
            base_url: format!("{}/v1", base_url.trim_end_matches('/')),
            api_format: format,
            api_key_env: None,
            headers: Vec::new(),
            models: Vec::new(),
            default_model: "test-model".into(),
            context_window: 100_000,
            max_output_tokens: 16_384,
        },
        auth,
        transport: waku_provider::TransportProfile::Standard,
        extra_auth_headers: Vec::new(),
    }
}

fn http_provider(config: ProviderConfig) -> HttpProvider {
    let providers = Providers::new();
    providers.set_providers(vec![config]).unwrap();
    HttpProvider::new(providers, "test-provider").unwrap()
}

#[tokio::test]
async fn openai_requests_include_service_tier() {
    for format in [ApiFormat::OpenAiResponses, ApiFormat::OpenAiChat] {
        let body = match format {
            ApiFormat::OpenAiResponses => sse(&[
                json!({"type":"response.created","response":{"id":"resp_tier"}}),
                json!({"type":"response.output_text.delta","delta":"ok"}),
                json!({"type":"response.completed","response":{"id":"resp_tier","usage":{"input_tokens":1,"output_tokens":1}}}),
            ]),
            ApiFormat::OpenAiChat => sse(&[
                json!({"id":"c","choices":[{"delta":{"content":"ok"}}]}),
                json!({"id":"c","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
            ]),
            ApiFormat::Anthropic => unreachable!(),
        };
        let server = spawn_mock(vec![ResponseSpec::ok(body)]).await;
        let auth = match format {
            ApiFormat::OpenAiResponses | ApiFormat::OpenAiChat => Auth::Bearer("sk".into()),
            ApiFormat::Anthropic => unreachable!(),
        };
        let provider = http_provider(provider_config(&server.base_url, format, auth));
        let mut sink = |_| {};
        provider
            .complete(
                &context_with_user("tier"),
                &RequestOptions {
                    max_tokens: Some(16),
                    temperature: None,
                    reasoning: None,
                    service_tier: Some(waku_provider::ServiceTier::Flex),
                    ..Default::default()
                },
                None,
                CancelToken::new(),
                &mut sink,
            )
            .await
            .unwrap();
        let request = &server.requests()[0];
        assert_eq!(request.body["service_tier"], "flex", "{format:?}");
    }
}

#[tokio::test]
async fn anthropic_rejects_service_tier() {
    let server = spawn_mock(vec![ResponseSpec::ok(String::new())]).await;
    let provider = http_provider(provider_config(
        &server.base_url,
        ApiFormat::Anthropic,
        Auth::AnthropicApiKey {
            key: "anthropic-secret".into(),
            version: "2023-06-01".into(),
        },
    ));
    let err = provider
        .complete(
            &context_with_user("tier"),
            &RequestOptions {
                max_tokens: Some(16),
                temperature: None,
                service_tier: Some(waku_provider::ServiceTier::Auto),
                ..Default::default()
            },
            None,
            CancelToken::new(),
            &mut |_| {},
        )
        .await
        .expect_err("anthropic must reject service tier");
    assert!(err.to_string().contains("service tier"));
    assert!(server.requests().is_empty());
}

fn context_with_user(text: &str) -> PromptContext {
    PromptContext {
        system_prompt: Some("system".into()),
        messages: vec![Message::User(UserMessage::text(text))],
        tools: Vec::new(),
    }
}

#[tokio::test]
async fn openai_responses_round_trip_builds_request_and_parses_sse() {
    let body = sse(&[
        json!({"type":"response.created","response":{"id":"resp_1"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"hello"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1"}}),
        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":9,"output_tokens":4,"total_tokens":13}}}),
    ]);
    let server = spawn_mock(vec![ResponseSpec::ok(body)]).await;
    let provider = http_provider(provider_config(
        &server.base_url,
        ApiFormat::OpenAiResponses,
        Auth::Bearer("responses-secret".into()),
    ));
    let context = context_with_user("hello");
    let options = RequestOptions {
        max_tokens: Some(32),
        temperature: Some(0.2),
        reasoning: Some("low".into()),
        service_tier: None,
        ..Default::default()
    };
    let mut events = Vec::new();
    let mut sink = |event| events.push(event);
    let message = provider
        .complete(&context, &options, None, CancelToken::new(), &mut sink)
        .await
        .unwrap();

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    assert_eq!(
        message.usage,
        Usage {
            input: 9,
            output: 4,
            cache_read: 0,
            cache_write: 0,
            reasoning: None,
            total_tokens: 13
        }
    );
    assert!(
        matches!(&message.content[0], ContentBlock::Text(TextBlock { text, .. }) if text == "hello")
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { delta, .. } if delta == "hello"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Done { .. }))
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/responses");
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer responses-secret")
    );
    assert_eq!(requests[0].body["model"], "test-model");
    assert_eq!(requests[0].body["input"][1]["content"][0]["text"], "hello");
    assert_eq!(requests[0].body["reasoning"]["effort"], "low");
    server.task.abort();
}

fn context_with_image() -> PromptContext {
    PromptContext {
        system_prompt: None,
        messages: vec![Message::User(waku_harness::UserMessage {
            parts: vec![
                waku_harness::UserPart::Text("look".into()),
                waku_harness::UserPart::Image {
                    mime_type: "image/png".into(),
                    data_b64: "aW1n".into(),
                },
            ],
        })],
        tools: Vec::new(),
    }
}

fn empty_terminal_sse(format: ApiFormat) -> String {
    match format {
        ApiFormat::OpenAiResponses => sse(&[
            json!({"type":"response.completed","response":{"id":"r","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
        ]),
        ApiFormat::OpenAiChat => {
            sse(&[
                json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
            ]) + "data: [DONE]\r\n\r\n"
        }
        ApiFormat::Anthropic => sse(&[
            json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ]),
    }
}

#[tokio::test]
async fn all_adapters_encode_user_images_in_the_request_body() {
    let cases = [
        (ApiFormat::OpenAiResponses, "/v1/responses"),
        (ApiFormat::OpenAiChat, "/v1/chat/completions"),
        (ApiFormat::Anthropic, "/v1/messages"),
    ];
    for (format, path) in cases {
        let server = spawn_mock(vec![ResponseSpec::ok(empty_terminal_sse(format))]).await;
        let provider = http_provider(provider_config(&server.base_url, format, Auth::None));
        let context = context_with_image();
        let mut sink = |_event| {};
        provider
            .complete(
                &context,
                &RequestOptions::default(),
                None,
                CancelToken::new(),
                &mut sink,
            )
            .await
            .unwrap();
        let body = &server.requests()[0].body;
        assert_eq!(server.requests()[0].path, path);
        let encoded = body.to_string();
        assert!(
            encoded.contains("aW1n"),
            "{format:?} request missing image payload: {encoded}"
        );
        match format {
            ApiFormat::OpenAiResponses => {
                assert!(encoded.contains("input_image") || encoded.contains("image_url"));
            }
            ApiFormat::OpenAiChat => {
                assert!(encoded.contains("image_url"));
            }
            ApiFormat::Anthropic => {
                assert!(encoded.contains("media_type") && encoded.contains("base64"));
            }
        }
        server.task.abort();
    }
}

#[tokio::test]
async fn openai_chat_round_trip_preserves_interleaved_tools_reasoning_and_usage() {
    let body = sse(&[
        json!({"id":"chat_1","choices":[{"delta":{"reasoning_content":"think"},"finish_reason":null}]}),
        json!({"id":"chat_1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-a","function":{"name":"first","arguments":r#"{"x":"#}}]},"finish_reason":null}]}),
        json!({"id":"chat_1","choices":[{"delta":{"tool_calls":[{"index":1,"id":"call-b","function":{"name":"second","arguments":r#"{"y":"#}}]},"finish_reason":null}]}),
        json!({"id":"chat_1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":null}],"usage":{"prompt_tokens":20,"completion_tokens":7,"total_tokens":27,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}}),
        json!({"id":"chat_1","choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"2}"}}]},"finish_reason":"tool_calls"}]}),
    ]);
    let server = spawn_mock(vec![ResponseSpec::ok(body)]).await;
    let provider = http_provider(provider_config(
        &server.base_url,
        ApiFormat::OpenAiChat,
        Auth::Bearer("chat-secret".into()),
    ));
    let context = context_with_user("call tools");
    let options = RequestOptions {
        max_tokens: Some(100),
        temperature: None,
        reasoning: Some("medium".into()),
        service_tier: None,
        ..Default::default()
    };
    let mut events = Vec::new();
    let mut sink = |event| events.push(event);
    let message = provider
        .complete(&context, &options, None, CancelToken::new(), &mut sink)
        .await
        .unwrap();

    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage.input, 17);
    assert_eq!(message.usage.output, 7);
    assert_eq!(message.usage.reasoning, Some(2));
    let calls: Vec<&ToolCall> = message.tool_calls().collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "first");
    assert_eq!(calls[0].arguments["x"], 1);
    assert_eq!(calls[1].name, "second");
    assert_eq!(calls[1].arguments["y"], 2);
    assert!(message.content.iter().any(|block| matches!(block, ContentBlock::Thinking(ThinkingBlock { thinking, .. }) if thinking == "think")));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::ThinkingEnd { .. }))
    );

    let requests = server.requests();
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer chat-secret")
    );
    assert_eq!(requests[0].body["stream_options"]["include_usage"], true);
    assert_eq!(requests[0].body["reasoning_effort"], "medium");
    server.task.abort();
}

#[tokio::test]
async fn anthropic_messages_round_trip_preserves_signature_usage_and_tool() {
    let body = sse(&[
        json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":7,"cache_read_input_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","signature":"initial"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"draft"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"final-signature"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":r#"{"path":"x""#}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ]);
    let server = spawn_mock(vec![ResponseSpec::ok(body)]).await;
    let mut config = provider_config(
        &server.base_url,
        ApiFormat::Anthropic,
        Auth::AnthropicApiKey {
            key: "anthropic-secret".into(),
            version: "2023-06-01".into(),
        },
    );
    config
        .endpoint
        .headers
        .push(("x-test-header".into(), "ok".into()));
    let provider = http_provider(config);
    let mut context = context_with_user("read file");
    context.tools.push(ToolSchema {
        name: "read".into(),
        description: "read a file".into(),
        parameters: json!({"type":"object"}),
    });
    let options = RequestOptions {
        max_tokens: Some(128),
        temperature: Some(0.1),
        reasoning: Some("high".into()),
        service_tier: None,
        ..Default::default()
    };
    let mut events = Vec::new();
    let mut sink = |event| events.push(event);
    let message = provider
        .complete(&context, &options, None, CancelToken::new(), &mut sink)
        .await
        .unwrap();

    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage.input, 7);
    assert_eq!(message.usage.cache_read, 1);
    assert_eq!(message.usage.output, 3);
    assert_eq!(message.usage.total_tokens, 11);
    assert!(
        matches!(&message.content[0], ContentBlock::Thinking(ThinkingBlock { thinking, signature, .. }) if thinking == "draft" && signature.as_deref() == Some("final-signature"))
    );
    let call = message.tool_calls().next().unwrap();
    assert_eq!(call.name, "read");
    assert_eq!(call.arguments["path"], "x");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::ThinkingEnd { .. }))
    );

    let requests = server.requests();
    assert_eq!(requests[0].path, "/v1/messages");
    assert_eq!(
        requests[0].headers.get("x-api-key").map(String::as_str),
        Some("anthropic-secret")
    );
    assert_eq!(
        requests[0]
            .headers
            .get("anthropic-version")
            .map(String::as_str),
        Some("2023-06-01")
    );
    assert_eq!(requests[0].body["max_tokens"], 128);
    assert_eq!(requests[0].body["thinking"]["type"], "adaptive");
    server.task.abort();
}

#[tokio::test]
async fn retry_after_retries_429_and_caps_server_delay_without_leaking_key() {
    let success =
        sse(&[json!({"id":"ok","choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})]);
    let server = spawn_mock(vec![
        ResponseSpec::error(
            429,
            vec![("retry-after-ms".into(), "1".into())],
            "bad retry responses-secret",
        ),
        ResponseSpec::ok(success),
    ])
    .await;
    let retry = waku_harness::RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_millis(1),
        max_retry_after: Duration::from_millis(20),
    };
    let provider = http_provider(provider_config(
        &server.base_url,
        ApiFormat::OpenAiChat,
        Auth::Bearer("responses-secret".into()),
    ))
    .with_retry(retry);
    let context = context_with_user("retry");
    let options = RequestOptions::default();
    let mut sink = |_| {};
    let message = provider
        .complete(&context, &options, None, CancelToken::new(), &mut sink)
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(server.requests().len(), 2);
    server.task.abort();

    let capped_server = spawn_mock(vec![ResponseSpec::error(
        429,
        vec![("retry-after-ms".into(), "100".into())],
        "contains responses-secret",
    )])
    .await;
    let retry = waku_harness::RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_retry_after: Duration::from_millis(1),
    };
    let provider = http_provider(provider_config(
        &capped_server.base_url,
        ApiFormat::OpenAiChat,
        Auth::Bearer("responses-secret".into()),
    ))
    .with_retry(retry);
    let mut sink = |_| {};
    let error = provider
        .complete(&context, &options, None, CancelToken::new(), &mut sink)
        .await
        .expect_err("expected an error");
    assert!(matches!(error, HarnessError::Http { status: 429, .. }));
    assert!(!error.to_string().contains("responses-secret"));
    assert_eq!(capped_server.requests().len(), 1);
    capped_server.task.abort();
}

#[tokio::test]
async fn http_errors_redact_configured_extra_header_values() {
    let gateway = "gateway-token-should-never-leak";
    let server = spawn_mock(vec![ResponseSpec::error(
        429,
        Vec::new(),
        format!("upstream echoed {gateway} and auth-secret"),
    )])
    .await;
    let mut config = provider_config(
        &server.base_url,
        ApiFormat::OpenAiChat,
        Auth::Bearer("auth-secret".into()),
    );
    config.endpoint.headers = vec![("x-gateway-token".into(), gateway.into())];
    let provider = http_provider(config).with_retry(waku_harness::RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_retry_after: Duration::from_millis(1),
    });
    let context = context_with_user("leak");
    let error = provider
        .complete(
            &context,
            &RequestOptions::default(),
            None,
            CancelToken::new(),
            &mut |_| {},
        )
        .await
        .expect_err("expected an http error");
    let rendered = error.to_string();
    assert!(
        matches!(error, HarnessError::Http { status: 429, .. }),
        "{error:?} / {rendered}"
    );
    assert!(!rendered.contains(gateway), "{rendered}");
    assert!(!rendered.contains("auth-secret"), "{rendered}");
    server.task.abort();
}

#[tokio::test]
async fn cancellation_interrupts_retry_delay_and_missing_terminal_is_error_state() {
    let server = spawn_mock(vec![ResponseSpec::error(
        429,
        vec![("retry-after-ms".into(), "5000".into())],
        "retry",
    )])
    .await;
    let retry = waku_harness::RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_millis(1),
        max_retry_after: Duration::from_secs(10),
    };
    let provider = http_provider(provider_config(
        &server.base_url,
        ApiFormat::OpenAiChat,
        Auth::None,
    ))
    .with_retry(retry);
    let context = context_with_user("cancel");
    let options = RequestOptions::default();
    let token = CancelToken::new();
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        let mut sink = |_| {};
        provider
            .complete(&context, &options, None, task_token, &mut sink)
            .await
    });
    sleep(Duration::from_millis(20)).await;
    token.cancel();
    let result = task.await.unwrap();
    assert!(matches!(result, Err(HarnessError::Cancelled)));
    server.task.abort();

    let missing = spawn_mock(vec![ResponseSpec::ok("data: {\"id\":\"partial\",\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\r\n")]).await;
    let provider = http_provider(provider_config(
        &missing.base_url,
        ApiFormat::OpenAiChat,
        Auth::None,
    ));
    let mut sink = |_| {};
    let message = provider
        .complete(
            &context_with_user("missing"),
            &RequestOptions::default(),
            None,
            CancelToken::new(),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("without a terminal"))
    );
    missing.task.abort();
}

struct ScriptedProvider {
    responses: Mutex<VecDeque<AssistantMessage>>,
    calls: AtomicUsize,
    seen_results: Mutex<Vec<Vec<(String, bool)>>>,
    seen_messages: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<AssistantMessage>) -> Self {
        ScriptedProvider {
            responses: Mutex::new(responses.into()),
            calls: AtomicUsize::new(0),
            seen_results: Mutex::new(Vec::new()),
            seen_messages: Mutex::new(Vec::new()),
        }
    }
}

impl ModelProvider for ScriptedProvider {
    fn complete<'a>(
        &'a self,
        ctx: &'a PromptContext,
        _opts: &'a RequestOptions,
        _model: Option<&'a str>,
        _cancel: CancelToken,
        sink: &'a mut (dyn FnMut(StreamEvent) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<AssistantMessage, HarnessError>> + Send + 'a>> {
        let response = self
            .responses
            .lock()
            .ok()
            .and_then(|mut responses| responses.pop_front())
            .unwrap();
        let results = ctx
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some((result.tool_call_id.clone(), result.is_error)),
                _ => None,
            })
            .collect();
        if let Ok(mut seen) = self.seen_results.lock() {
            seen.push(results);
        }
        if let Ok(mut seen) = self.seen_messages.lock() {
            seen.push(ctx.messages.clone());
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            sink(StreamEvent::Start);
            sink(StreamEvent::Done {
                usage: response.usage,
                stop_reason: response.stop_reason,
            });
            Ok(response)
        })
    }
}

#[derive(Clone)]
struct DelayTool {
    name: &'static str,
    spec: ToolSpec,
    delay: Duration,
    terminate: bool,
    calls: Arc<AtomicUsize>,
}

impl DelayTool {
    fn new(name: &'static str, delay: Duration, terminate: bool, calls: Arc<AtomicUsize>) -> Self {
        Self {
            name,
            spec: ToolSpec {
                name: name.into(),
                description: "test tool".into(),
                parameters: json!({"type": "object"}),
                required: Vec::new(),
            },
            delay,
            terminate,
            calls,
        }
    }
}

impl Tool for DelayTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(
        &'a self,
        _call: &'a ToolCall,
        exec: ExecutionContext<'a>,
    ) -> BoxFuture<'a, Result<ExecOutcome, ToolError>> {
        Box::pin(async move {
            exec.cancel
                .race_delay(self.delay)
                .await
                .map_err(|_| ToolError::Cancelled)?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ExecOutcome {
                parts: vec![waku_harness::ToolResultPart::Text(self.name.into())],
                details: None,
                terminate: self.terminate,
            })
        })
    }
}

fn assistant_with_tool_calls(stop_reason: StopReason, names: &[(&str, &str)]) -> AssistantMessage {
    AssistantMessage {
        content: names
            .iter()
            .map(|(id, name)| {
                ContentBlock::ToolCall(Arc::new(ToolCall {
                    id: (*id).into(),
                    name: (*name).into(),
                    arguments: json!({}),
                    thought_signature: None,
                }))
            })
            .collect(),
        model: "scripted".into(),
        provider: "scripted".into(),
        response_id: None,
        usage: Usage::default(),
        stop_reason,
        error_message: None,
    }
}

fn final_assistant() -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextBlock {
            text: "done".into(),
            signature: None,
        })],
        model: "scripted".into(),
        provider: "scripted".into(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
    }
}

#[tokio::test]
async fn agent_executes_tools_concurrently_but_records_results_in_source_order() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        assistant_with_tool_calls(StopReason::ToolUse, &[("a", "slow"), ("b", "fast")]),
        final_assistant(),
    ]));
    let calls = Arc::new(AtomicUsize::new(0));
    let harness = Harness::new(provider.clone()).with_tools(vec![
        Arc::new(DelayTool::new(
            "slow",
            Duration::from_millis(30),
            false,
            Arc::clone(&calls),
        )),
        Arc::new(DelayTool::new(
            "fast",
            Duration::from_millis(1),
            false,
            Arc::clone(&calls),
        )),
    ]);
    let mut session = waku_harness::Session::new(None);
    let mut events = Vec::new();
    let outcome = harness
        .run(&mut session, "go", CancelToken::new(), |event| {
            events.push(event)
        })
        .await
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Completed));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let finished: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolFinished { result } => Some(result.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(finished, vec!["b", "a"]);
    let seen = provider.seen_results.lock().unwrap();
    assert_eq!(seen[1], vec![("a".into(), false), ("b".into(), false)]);
}

#[tokio::test]
async fn agent_emits_tool_finished_in_completion_order_before_slow_tools_end() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        assistant_with_tool_calls(StopReason::ToolUse, &[("a", "slow"), ("b", "fast")]),
        final_assistant(),
    ]));
    let harness = Harness::new(provider).with_tools(vec![
        Arc::new(DelayTool::new(
            "slow",
            Duration::from_millis(80),
            false,
            Arc::new(AtomicUsize::new(0)),
        )),
        Arc::new(DelayTool::new(
            "fast",
            Duration::from_millis(1),
            false,
            Arc::new(AtomicUsize::new(0)),
        )),
    ]);
    let mut session = waku_harness::Session::new(None);
    let (tx, rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    harness
        .run(&mut session, "go", CancelToken::new(), move |event| {
            if let AgentEvent::ToolFinished { result } = &event {
                let _ = tx.send((result.tool_call_id.clone(), started.elapsed()));
            }
        })
        .await
        .unwrap();
    let first = rx.recv().unwrap();
    assert_eq!(first.0, "b");
    assert!(
        first.1 < Duration::from_millis(40),
        "fast tool should finish before the slow tool, got {:?}",
        first.1
    );
}

#[tokio::test]
async fn agent_tool_finished_carries_exact_success_and_failure_results() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        assistant_with_tool_calls(StopReason::ToolUse, &[("ok", "fast"), ("bad", "missing")]),
        final_assistant(),
    ]));
    let calls = Arc::new(AtomicUsize::new(0));
    let harness = Harness::new(provider.clone()).with_tools(vec![Arc::new(DelayTool::new(
        "fast",
        Duration::from_millis(1),
        false,
        Arc::clone(&calls),
    ))]);
    let mut session = waku_harness::Session::new(None);
    let mut events = Vec::new();

    let outcome = harness
        .run(&mut session, "go", CancelToken::new(), |event| {
            events.push(event)
        })
        .await
        .unwrap();

    assert!(matches!(outcome, RunOutcome::Completed));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let success = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolFinished { result } if result.tool_call_id == "ok" => Some(result),
            _ => None,
        })
        .expect("successful completion event");
    assert_eq!(success.tool_call_id, "ok");
    assert_eq!(success.tool_name, "fast");
    assert!(!success.is_error);
    assert!(matches!(
        success.content.as_slice(),
        [waku_harness::ToolResultPart::Text(text)] if text == "fast"
    ));

    let failure = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolFinished { result } if result.tool_call_id == "bad" => Some(result),
            _ => None,
        })
        .expect("failed completion event");
    assert_eq!(failure.tool_call_id, "bad");
    assert_eq!(failure.tool_name, "missing");
    assert!(failure.is_error);
    assert!(matches!(
        failure.content.as_slice(),
        [waku_harness::ToolResultPart::Text(text)] if text.contains("unknown tool: missing")
    ));

    let seen = provider.seen_results.lock().unwrap();
    assert_eq!(seen[1], vec![("ok".into(), false), ("bad".into(), true)]);

    let transcript_success = session
        .transcript()
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "ok" => Some(result),
            _ => None,
        })
        .expect("successful transcript result");
    let transcript_failure = session
        .transcript()
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "bad" => Some(result),
            _ => None,
        })
        .expect("failed transcript result");
    assert_eq!(success.tool_name, transcript_success.tool_name);
    assert_eq!(success.content, transcript_success.content);
    assert_eq!(failure.tool_name, transcript_failure.tool_name);
    assert_eq!(failure.content, transcript_failure.content);
    assert!(std::sync::Arc::ptr_eq(success, transcript_success));
    assert!(std::sync::Arc::ptr_eq(failure, transcript_failure));
}

#[tokio::test]
async fn agent_requires_all_terminate_hints_and_rejects_all_length_calls() {
    let terminate_provider = Arc::new(ScriptedProvider::new(vec![assistant_with_tool_calls(
        StopReason::ToolUse,
        &[("a", "first"), ("b", "second")],
    )]));
    let terminate_calls = Arc::new(AtomicUsize::new(0));
    let harness = Harness::new(terminate_provider.clone()).with_tools(vec![
        Arc::new(DelayTool::new(
            "first",
            Duration::from_millis(1),
            true,
            Arc::clone(&terminate_calls),
        )),
        Arc::new(DelayTool::new(
            "second",
            Duration::from_millis(1),
            true,
            Arc::clone(&terminate_calls),
        )),
    ]);
    let mut session = waku_harness::Session::new(None);
    let outcome = harness
        .run(&mut session, "stop", CancelToken::new(), |_| {})
        .await
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Completed));
    assert_eq!(terminate_provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(terminate_calls.load(Ordering::SeqCst), 2);

    let length_provider = Arc::new(ScriptedProvider::new(vec![
        assistant_with_tool_calls(StopReason::Length, &[("a", "first"), ("b", "second")]),
        final_assistant(),
    ]));
    let length_calls = Arc::new(AtomicUsize::new(0));
    let harness = Harness::new(length_provider.clone()).with_tools(vec![
        Arc::new(DelayTool::new(
            "first",
            Duration::from_millis(1),
            false,
            Arc::clone(&length_calls),
        )),
        Arc::new(DelayTool::new(
            "second",
            Duration::from_millis(1),
            false,
            Arc::clone(&length_calls),
        )),
    ]);
    let mut session = waku_harness::Session::new(None);
    let outcome = harness
        .run(&mut session, "length", CancelToken::new(), |_| {})
        .await
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Completed));
    assert_eq!(length_calls.load(Ordering::SeqCst), 0);
    let seen = length_provider.seen_results.lock().unwrap();
    assert_eq!(seen[1], vec![("a".into(), true), ("b".into(), true)]);
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_root() -> PathBuf {
    let number = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("waku-harness-{}-{number}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn run_tool<T: Tool>(
    tool: &T,
    ctx: &ToolContext,
    call: ToolCall,
) -> Result<ExecOutcome, ToolError> {
    futures::executor::block_on(tool.execute(
        &call,
        ExecutionContext {
            ctx,
            cancel: CancelToken::new(),
        },
    ))
}

#[test]
fn built_in_tools_execute_and_reject_path_traversal() {
    let root = temp_root();
    let context = ToolContext::new(&root);
    let write = waku_harness::WriteTool::unbound();
    let read = waku_harness::ReadTool::unbound();
    let edit = waku_harness::EditTool::unbound();
    let list = waku_harness::ListTool::unbound();
    let search = waku_harness::SearchTool::unbound();
    let shell = waku_harness::ShellTool::unbound().with_timeout(Duration::from_secs(2));

    let write_result = run_tool(
        &write,
        &context,
        ToolCall {
            id: "w".into(),
            name: "write".into(),
            arguments: json!({"path":"nested/file.txt","content":"alpha\nbeta\n"}),
            thought_signature: None,
        },
    )
    .unwrap();
    assert!(
        matches!(&write_result.parts[0], waku_harness::ToolResultPart::Text(text) if text.contains("wrote"))
    );
    let read_result = run_tool(
        &read,
        &context,
        ToolCall {
            id: "r".into(),
            name: "read".into(),
            arguments: json!({"path":"nested/file.txt"}),
            thought_signature: None,
        },
    )
    .unwrap();
    assert!(
        matches!(&read_result.parts[0], waku_harness::ToolResultPart::Text(text) if text.contains("alpha") && text.contains("beta"))
    );
    run_tool(
        &edit,
        &context,
        ToolCall {
            id: "e".into(),
            name: "edit".into(),
            arguments: json!({"path":"nested/file.txt","old":"alpha","new":"gamma"}),
            thought_signature: None,
        },
    )
    .unwrap();
    let list_result = run_tool(
        &list,
        &context,
        ToolCall {
            id: "l".into(),
            name: "list".into(),
            arguments: json!({"path":"nested"}),
            thought_signature: None,
        },
    )
    .unwrap();
    assert!(
        matches!(&list_result.parts[0], waku_harness::ToolResultPart::Text(text) if text.contains("file.txt"))
    );
    let search_result = run_tool(
        &search,
        &context,
        ToolCall {
            id: "s".into(),
            name: "search".into(),
            arguments: json!({"pattern":"gamma","path":"."}),
            thought_signature: None,
        },
    )
    .unwrap();
    assert!(
        matches!(&search_result.parts[0], waku_harness::ToolResultPart::Text(text) if text.contains("file.txt"))
    );
    let shell_result = run_tool(
        &shell,
        &context,
        ToolCall {
            id: "sh".into(),
            name: "shell".into(),
            arguments: json!({"command":"printf shell-ok"}),
            thought_signature: None,
        },
    )
    .unwrap();
    assert!(
        matches!(&shell_result.parts[0], waku_harness::ToolResultPart::Text(text) if text.contains("shell-ok"))
    );

    let traversal = run_tool(
        &write,
        &context,
        ToolCall {
            id: "bad".into(),
            name: "write".into(),
            arguments: json!({"path":"../escape.txt","content":"escape"}),
            thought_signature: None,
        },
    )
    .err()
    .expect("expected an error");
    assert!(
        matches!(traversal, ToolError::InvalidArguments(message) if message.contains("parent traversal"))
    );
    let traversal = run_tool(
        &read,
        &context,
        ToolCall {
            id: "bad-read".into(),
            name: "read".into(),
            arguments: json!({"path":"../missing.txt"}),
            thought_signature: None,
        },
    )
    .err()
    .expect("expected an error");
    assert!(matches!(traversal, ToolError::InvalidArguments(_)));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = root
            .parent()
            .unwrap()
            .join(format!("waku-harness-outside-{}", std::process::id()));
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, root.join("link.txt")).unwrap();
        let symlink_read = run_tool(
            &read,
            &context,
            ToolCall {
                id: "link".into(),
                name: "read".into(),
                arguments: json!({"path":"link.txt"}),
                thought_signature: None,
            },
        )
        .err()
        .expect("expected an error");
        assert!(
            matches!(symlink_read, ToolError::InvalidArguments(message) if message.contains("outside allowed roots"))
        );
        let _ = std::fs::remove_file(outside);
    }
    let _ = std::fs::remove_dir_all(root);
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User(message) => Some(UserMessage::text_of(&message.parts)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn session_steering_injects_each_message_before_its_llm_call() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        final_assistant(),
        final_assistant(),
    ]));
    let harness = Harness::new(provider.clone());
    let mut session = Session::new(None).with_queue_mode(QueueMode::OneAtATime);
    let handle: SessionSteering = session.steering().clone();
    let (first_id, second_id) = std::thread::spawn(move || {
        let first_id = handle.steer_text("follow-up one");
        let second_id = handle.steer(UserMessage::text("follow-up two"));
        (first_id, second_id)
    })
    .join()
    .unwrap();

    let mut events = Vec::new();
    let outcome = harness
        .continue_run(&mut session, CancelToken::new(), |event| events.push(event))
        .await
        .unwrap();

    assert!(matches!(outcome, RunOutcome::Completed));
    let injected: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SteeringInjected { id } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(injected, vec![first_id, second_id]);

    let first_injected = events
        .iter()
        .position(|event| matches!(event, AgentEvent::SteeringInjected { id } if *id == first_id))
        .unwrap();
    let second_injected = events
        .iter()
        .position(|event| matches!(event, AgentEvent::SteeringInjected { id } if *id == second_id))
        .unwrap();
    let turns: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| matches!(event, AgentEvent::TurnStarted).then_some(index))
        .collect();
    assert_eq!(turns.len(), 2);
    assert!(first_injected < turns[0]);
    assert!(turns[0] < second_injected);
    assert!(second_injected < turns[1]);

    let seen = provider.seen_messages.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(user_texts(&seen[0]), vec!["follow-up one"]);
    assert_eq!(user_texts(&seen[1]), vec!["follow-up one", "follow-up two"]);
    assert_eq!(session.queue_len(), 0);
}

#[tokio::test]
async fn steering_queued_at_run_end_is_preserved_for_the_next_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        final_assistant(),
        final_assistant(),
    ]));
    let harness = Harness::new(provider.clone());
    let mut session = Session::new(None);
    let steering = session.steering();
    let queued_id = Arc::new(Mutex::new(None));
    let queued_id_from_sink = Arc::clone(&queued_id);

    harness
        .run_text(&mut session, "first", CancelToken::new(), move |event| {
            if matches!(event, AgentEvent::RunEnded { .. }) {
                let id = steering.steer_text("follow-up after run end");
                *queued_id_from_sink.lock().unwrap() = Some(id);
            }
        })
        .await
        .unwrap();

    let queued_id = queued_id.lock().unwrap().expect("follow-up was queued");
    assert_eq!(session.queue_len(), 1);

    let mut events = Vec::new();
    harness
        .continue_run(&mut session, CancelToken::new(), |event| events.push(event))
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::SteeringInjected { id } if *id == queued_id))
    );
    assert_eq!(session.queue_len(), 0);

    let seen = provider.seen_messages.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        user_texts(&seen[1]),
        vec!["first", "follow-up after run end"]
    );
}

#[tokio::test]
async fn session_checkpoints_truncate_and_fork_completed_turns_exactly() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        final_assistant(),
        final_assistant(),
    ]));
    let harness = Harness::new(provider);
    let budget = Budget {
        max_messages: Some(32),
        max_tokens: Some(4_096),
    };
    let mut session = Session::new(Some("system".into()))
        .with_queue_mode(QueueMode::All)
        .with_budget(budget.clone());

    harness
        .run_text(&mut session, "first", CancelToken::new(), |_| {})
        .await
        .unwrap();
    let first_checkpoint = session.record_turn_checkpoint();
    assert_eq!(first_checkpoint.system_prompt.as_deref(), Some("system"));
    assert_eq!(first_checkpoint.queue_mode, QueueMode::All);
    assert_eq!(&first_checkpoint.budget, &budget);
    assert_eq!(user_texts(first_checkpoint.transcript()), vec!["first"]);
    assert_eq!(first_checkpoint.checkpoints.len(), 1);
    assert_eq!(
        first_checkpoint.checkpoints[0].message_count,
        first_checkpoint.messages.len()
    );

    harness
        .run_text(&mut session, "second", CancelToken::new(), |_| {})
        .await
        .unwrap();
    session.record_turn_checkpoint();
    assert_eq!(session.completed_turn_count(), 2);
    assert_eq!(user_texts(session.transcript()), vec!["first", "second"]);

    let snapshot = session.snapshot();
    assert_eq!(snapshot.checkpoints.len(), 2);
    assert_eq!(snapshot.initial_checkpoint.message_count, 0);
    let restored = Session::with_snapshot(snapshot.clone()).unwrap();
    assert_eq!(restored.completed_turn_count(), 2);
    assert_eq!(restored.queue_len(), 0);
    assert_eq!(user_texts(restored.transcript()), vec!["first", "second"]);

    let boundaries: Vec<_> = snapshot
        .checkpoints
        .iter()
        .map(|checkpoint| checkpoint.message_count)
        .collect();
    let restored_from_boundaries = Session::with_history(
        snapshot.system_prompt.clone(),
        snapshot.messages.clone(),
        boundaries.clone(),
        snapshot.queue_mode,
        snapshot.budget.clone(),
    )
    .unwrap();
    assert_eq!(restored_from_boundaries.completed_turn_count(), 2);
    assert_eq!(
        user_texts(restored_from_boundaries.transcript()),
        vec!["first", "second"]
    );

    assert!(matches!(
        Session::with_history(
            snapshot.system_prompt.clone(),
            snapshot.messages.clone(),
            vec![boundaries[0], boundaries[0]],
            snapshot.queue_mode,
            snapshot.budget.clone(),
        ),
        Err(HarnessError::InvalidRequest(_))
    ));
    let mut overlapping = snapshot.clone();
    overlapping.checkpoints[1].message_count = overlapping.checkpoints[0].message_count;
    assert!(matches!(
        Session::with_snapshot(overlapping),
        Err(HarnessError::InvalidRequest(_))
    ));

    let handle = session.steering();
    handle.steer_text("pending");
    assert_eq!(session.queue_len(), 1);
    let cloned = session.clone();
    assert_eq!(cloned.queue_len(), 0);

    let fork = session.fork_completed_turns(1).unwrap();
    assert_eq!(fork.completed_turn_count(), 1);
    assert_eq!(fork.queue_len(), 0);
    assert_eq!(fork.system_prompt(), Some("system"));
    assert_eq!(user_texts(fork.transcript()), vec!["first"]);

    assert!(matches!(
        session.fork_completed_turns(3),
        Err(HarnessError::InvalidRequest(_))
    ));
    assert!(matches!(
        session.truncate_completed_turns(3),
        Err(HarnessError::InvalidRequest(_))
    ));

    session.truncate_completed_turns(1).unwrap();
    assert_eq!(session.completed_turn_count(), 1);
    assert_eq!(session.queue_len(), 0);
    assert_eq!(user_texts(session.transcript()), vec!["first"]);

    session.truncate_completed_turns(0).unwrap();
    assert_eq!(session.completed_turn_count(), 0);
    assert!(session.transcript().is_empty());
    assert_eq!(session.system_prompt(), Some("system"));
    assert_eq!(session.snapshot().queue_mode, QueueMode::All);
    assert_eq!(&session.snapshot().budget, &budget);
}

#[derive(Clone)]
struct ApprovalMockTool {
    calls: Arc<AtomicUsize>,
}

impl Tool for ApprovalMockTool {
    fn name(&self) -> &'static str {
        "approval-mock"
    }

    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::LazyLock<ToolSpec> = std::sync::LazyLock::new(|| ToolSpec {
            name: "approval-mock".into(),
            description: "approval test tool".into(),
            parameters: json!({"type":"object"}),
            required: vec!["ok".into()],
        });
        &SPEC
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn execute<'a>(
        &'a self,
        _call: &'a ToolCall,
        _exec: ExecutionContext<'a>,
    ) -> BoxFuture<'a, Result<ExecOutcome, ToolError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ExecOutcome::text("side effect"))
        })
    }
}

#[derive(Clone)]
struct FixedApprovalGate {
    decision: ApprovalDecision<()>,
}

impl ApprovalGate<ToolCall> for FixedApprovalGate {
    type Approved = ();

    fn approve<'a>(
        &'a self,
        request: ApprovalRequest<ToolCall>,
    ) -> BoxFuture<'a, Result<ApprovalDecision<Self::Approved>, ToolError>> {
        assert_eq!(request.value.name, "approval-mock");
        let decision = self.decision.clone();
        Box::pin(async move { Ok(decision) })
    }
}

struct WaitingApprovalGate {
    started: std::sync::mpsc::Sender<()>,
}

impl ApprovalGate<ToolCall> for WaitingApprovalGate {
    type Approved = ();

    fn approve<'a>(
        &'a self,
        request: ApprovalRequest<ToolCall>,
    ) -> BoxFuture<'a, Result<ApprovalDecision<Self::Approved>, ToolError>> {
        let _ = self.started.send(());
        Box::pin(async move {
            request.cancel.cancelled().await;
            Ok(ApprovalDecision::Approved(()))
        })
    }
}

fn approval_call() -> ToolCall {
    ToolCall {
        id: "approval".into(),
        name: "approval-mock".into(),
        arguments: json!({"ok":true}),
        thought_signature: None,
    }
}

#[test]
fn approval_tool_allows_denies_and_cancels_before_mock_side_effects() {
    let context = ToolContext::new(".");
    let calls = Arc::new(AtomicUsize::new(0));
    let allowed = ApprovalTool::new(
        ApprovalMockTool {
            calls: Arc::clone(&calls),
        },
        FixedApprovalGate {
            decision: ApprovalDecision::Approved(()),
        },
    );
    assert_eq!(allowed.name(), "approval-mock");
    assert_eq!(allowed.spec().name, "approval-mock");
    assert_eq!(allowed.execution_mode(), ExecutionMode::Sequential);
    assert!(allowed.validate(&json!({"ok":true})).is_ok());
    assert!(matches!(
        allowed.validate(&json!({})),
        Err(ToolError::InvalidArguments(_))
    ));
    run_tool(&allowed, &context, approval_call()).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let denied = ApprovalTool::new(
        ApprovalMockTool {
            calls: Arc::clone(&calls),
        },
        FixedApprovalGate {
            decision: ApprovalDecision::Denied,
        },
    );
    assert!(matches!(
        run_tool(&denied, &context, approval_call()),
        Err(ToolError::ApprovalDenied)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let cancelled = ApprovalTool::new(
        ApprovalMockTool {
            calls: Arc::clone(&calls),
        },
        FixedApprovalGate {
            decision: ApprovalDecision::Cancelled,
        },
    );
    assert!(matches!(
        run_tool(&cancelled, &context, approval_call()),
        Err(ToolError::Cancelled)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (started, waiting) = std::sync::mpsc::channel();
    let cancel = CancelToken::new();
    let cancel_for_worker = cancel.clone();
    let calls_for_worker = Arc::clone(&calls);
    let worker = std::thread::spawn(move || {
        let tool = ApprovalTool::new(
            ApprovalMockTool {
                calls: calls_for_worker,
            },
            WaitingApprovalGate { started },
        );
        let context = ToolContext::new(".");
        let call = approval_call();
        futures::executor::block_on(tool.execute(
            &call,
            ExecutionContext {
                ctx: &context,
                cancel: cancel_for_worker,
            },
        ))
    });
    waiting.recv_timeout(Duration::from_secs(1)).unwrap();
    cancel.cancel();
    assert!(matches!(worker.join().unwrap(), Err(ToolError::Cancelled)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
