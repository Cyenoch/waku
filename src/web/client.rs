//! Browser WebSocket transport for the shared WakuWaku protocol.
//!
//! The browser client keeps all socket state on the wasm event loop. Socket
//! callbacks hold a `Weak` reference so the callback/socket cycle is broken
//! when the last client is dropped.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::mem;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;
use futures::future::{Either, select};
use gpui::{BackgroundExecutor, ForegroundExecutor, Task};
use url::Url;
use uuid::Uuid;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wakuwaku_protocol::{
    ClientMessage, Command, PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome,
    ResponsePayload, RpcError, SequencedEvent, ServerMessage,
};

/// The nil UUID used for daemon-global requests.
pub const NIL_UUID: Uuid = Uuid::nil();

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BUFFERED_EVENTS_PER_RUNTIME: usize = 4096;

type RuntimeKey = (Uuid, Uuid);
type PendingRequest = oneshot::Sender<Result<ResponsePayload, RpcError>>;

#[derive(Clone, Copy)]
struct LastSequence {
    epoch: Uuid,
    sequence: u64,
}

struct Inner {
    socket: web_sys::WebSocket,
    foreground: ForegroundExecutor,
    background: BackgroundExecutor,
    connected: bool,
    closed: bool,
    handshake: Option<oneshot::Sender<Result<(), String>>>,
    pending: HashMap<Uuid, PendingRequest>,
    subscribers: HashMap<RuntimeKey, Vec<UnboundedSender<SequencedEvent>>>,
    buffered: HashMap<RuntimeKey, VecDeque<SequencedEvent>>,
    last_sequences: HashMap<RuntimeKey, LastSequence>,
    on_open: Option<Closure<dyn FnMut(web_sys::Event)>>,
    on_message: Option<Closure<dyn FnMut(web_sys::MessageEvent)>>,
    on_error: Option<Closure<dyn FnMut(web_sys::Event)>>,
    on_close: Option<Closure<dyn FnMut(web_sys::CloseEvent)>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onerror(None);
        self.socket.set_onclose(None);
    }
}

/// Browser-side client for WakuWaku's versioned JSON-over-WebSocket protocol.
#[derive(Clone)]
pub struct WebDaemonClient(Rc<RefCell<Inner>>);

impl WebDaemonClient {
    /// Open a daemon WebSocket and complete its protocol handshake.
    pub fn connect(
        address: &str,
        token: String,
        resume_from: Vec<ReplayCursor>,
        foreground: ForegroundExecutor,
        background: BackgroundExecutor,
    ) -> Task<anyhow::Result<Self>> {
        let address = address.to_owned();
        let task_foreground = foreground.clone();
        task_foreground.spawn(async move {
            connect_inner(address, token, resume_from, foreground, background).await
        })
    }

    /// Send an RPC request and resolve it with the daemon response payload.
    pub fn request(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> Task<Result<ResponsePayload, RpcError>> {
        let request_id = Uuid::new_v4();
        let message = ClientMessage::Request(Request {
            request_id,
            session_id,
            runtime_id,
            command,
        });
        let payload = match serde_json::to_string(&message) {
            Ok(payload) => payload,
            Err(error) => {
                return failed_request_task(
                    self.0.borrow().foreground.clone(),
                    format!("could not encode WakuWaku request: {error}"),
                );
            }
        };

        let (response_sender, response_receiver) = oneshot::channel();
        let (foreground, background, send_error) = {
            let mut inner = self.0.borrow_mut();
            let foreground = inner.foreground.clone();
            let background = inner.background.clone();
            if inner.closed
                || !inner.connected
                || inner.socket.ready_state() != web_sys::WebSocket::OPEN
            {
                return failed_request_task(
                    foreground,
                    "WakuWaku daemon is disconnected".to_owned(),
                );
            }

            inner.pending.insert(request_id, response_sender);
            let send_error = inner
                .socket
                .send_with_str(&payload)
                .err()
                .map(|error| format!("could not send WakuWaku request: {error:?}"));
            (foreground, background, send_error)
        };

        if let Some(error) = send_error {
            fail_connection(&self.0, error.clone(), Some((1011, "send failed")));
            return failed_request_task(foreground, error);
        }

        let shared = self.0.clone();
        foreground.spawn(async move {
            let timeout = background.timer(REQUEST_TIMEOUT);
            futures::pin_mut!(response_receiver, timeout);
            match select(response_receiver, timeout).await {
                Either::Left((Ok(result), _)) => result,
                Either::Left((Err(_), _)) => Err(RpcError {
                    message: "WakuWaku daemon disconnected".to_owned(),
                }),
                Either::Right(((), _)) => {
                    remove_pending(&shared, request_id);
                    Err(RpcError {
                        message: "timed out waiting for WakuWaku daemon".to_owned(),
                    })
                }
            }
        })
    }

    /// Register a stream for one `(session_id, runtime_id)` pair.
    pub fn subscribe(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
    ) -> UnboundedReceiver<SequencedEvent> {
        let key = (session_id, runtime_id);
        let (sender, receiver) = unbounded();
        let mut inner = self.0.borrow_mut();

        if let Some(buffered) = inner.buffered.remove(&key) {
            for event in buffered {
                let _ = sender.unbounded_send(event);
            }
        }
        inner.subscribers.entry(key).or_default().push(sender);
        receiver
    }

    /// Whether the handshake completed and the WebSocket remains open.
    pub fn connected(&self) -> bool {
        let inner = self.0.borrow();
        inner.connected
            && !inner.closed
            && inner.socket.ready_state() == web_sys::WebSocket::OPEN
    }
}

async fn connect_inner(
    address: String,
    token: String,
    resume_from: Vec<ReplayCursor>,
    foreground: ForegroundExecutor,
    background: BackgroundExecutor,
) -> anyhow::Result<WebDaemonClient> {
    let address = daemon_url(&address)?;
    let socket = web_sys::WebSocket::new(&address)
        .map_err(|error| anyhow!("could not connect to WakuWaku daemon: {error:?}"))?;
    let hello = serde_json::to_string(&ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        token,
        client_id: Uuid::new_v4(),
        resume_from: resume_from.clone(),
    })?;
    let (handshake_sender, handshake_receiver) = oneshot::channel();

    let last_sequences = resume_from
        .into_iter()
        .map(|cursor| {
            (
                (cursor.session_id, cursor.runtime_id),
                LastSequence {
                    epoch: cursor.epoch,
                    sequence: cursor.sequence,
                },
            )
        })
        .collect();

    let inner = Rc::new(RefCell::new(Inner {
        socket: socket.clone(),
        foreground,
        background,
        connected: false,
        closed: false,
        handshake: Some(handshake_sender),
        pending: HashMap::new(),
        subscribers: HashMap::new(),
        buffered: HashMap::new(),
        last_sequences,
        on_open: None,
        on_message: None,
        on_error: None,
        on_close: None,
    }));

    install_handlers(&inner, hello);
    let client = WebDaemonClient(inner.clone());

    match handshake_receiver.await {
        Ok(Ok(())) if client.connected() => Ok(client),
        Ok(Ok(())) => Err(anyhow!("WakuWaku daemon disconnected during handshake")),
        Ok(Err(error)) => Err(anyhow!(error)),
        Err(_) => Err(anyhow!("WakuWaku daemon handshake was cancelled")),
    }
}

fn install_handlers(inner: &Rc<RefCell<Inner>>, hello: String) {
    let weak = Rc::downgrade(inner);
    let on_open = Closure::wrap(Box::new(move |_event: web_sys::Event| {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let socket = {
            let state = inner.borrow();
            if state.closed {
                return;
            }
            state.socket.clone()
        };
        if let Err(error) = socket.send_with_str(&hello) {
            fail_connection(
                &inner,
                format!("could not send WakuWaku handshake: {error:?}"),
                Some((1002, "handshake failed")),
            );
        }
    }) as Box<dyn FnMut(web_sys::Event)>);

    let weak = Rc::downgrade(inner);
    let on_message = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let Some(data) = event.data().as_string() else {
            fail_connection(
                &inner,
                "WakuWaku daemon sent a non-text WebSocket message".to_owned(),
                Some((1002, "text messages required")),
            );
            return;
        };
        handle_message(&inner, &data);
    }) as Box<dyn FnMut(web_sys::MessageEvent)>);

    let weak = Rc::downgrade(inner);
    let on_error = Closure::wrap(Box::new(move |_event: web_sys::Event| {
        if let Some(inner) = weak.upgrade() {
            fail_connection(
                &inner,
                "WakuWaku daemon WebSocket connection failed".to_owned(),
                None,
            );
        }
    }) as Box<dyn FnMut(web_sys::Event)>);

    let weak = Rc::downgrade(inner);
    let on_close = Closure::wrap(Box::new(move |_event: web_sys::CloseEvent| {
        if let Some(inner) = weak.upgrade() {
            fail_connection(
                &inner,
                "WakuWaku daemon WebSocket closed".to_owned(),
                None,
            );
        }
    }) as Box<dyn FnMut(web_sys::CloseEvent)>);

    let mut state = inner.borrow_mut();
    state.socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    state
        .socket
        .set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    state
        .socket
        .set_onerror(Some(on_error.as_ref().unchecked_ref()));
    state
        .socket
        .set_onclose(Some(on_close.as_ref().unchecked_ref()));
    state.on_open = Some(on_open);
    state.on_message = Some(on_message);
    state.on_error = Some(on_error);
    state.on_close = Some(on_close);
}

fn handle_message(inner: &Rc<RefCell<Inner>>, data: &str) {
    let message = match serde_json::from_str::<ServerMessage>(data) {
        Ok(message) => message,
        Err(error) => {
            fail_connection(
                inner,
                format!("WakuWaku daemon sent invalid JSON: {error}"),
                Some((1002, "invalid JSON")),
            );
            return;
        }
    };

    let handshaking = inner.borrow().handshake.is_some();
    if handshaking {
        handle_handshake(inner, message);
    } else if !inner.borrow().closed {
        dispatch_message(inner, message);
    }
}

fn handle_handshake(inner: &Rc<RefCell<Inner>>, message: ServerMessage) {
    match message {
        ServerMessage::Hello {
            protocol_version, ..
        } if protocol_version == PROTOCOL_VERSION => {
            let sender = {
                let mut state = inner.borrow_mut();
                if state.closed {
                    return;
                }
                state.connected = true;
                state.handshake.take()
            };
            if let Some(sender) = sender {
                let _ = sender.send(Ok(()));
            }
        }
        ServerMessage::Hello {
            protocol_version, ..
        } => fail_connection(
            inner,
            format!(
                "daemon protocol {protocol_version} does not match client protocol {PROTOCOL_VERSION}"
            ),
            Some((1002, "protocol version mismatch")),
        ),
        ServerMessage::Rejected { message } => fail_connection(
            inner,
            format!("daemon rejected connection: {message}"),
            Some((1008, "handshake rejected")),
        ),
        _ => fail_connection(
            inner,
            "WakuWaku daemon sent an invalid handshake response".to_owned(),
            Some((1002, "invalid handshake")),
        ),
    }
}

fn dispatch_message(inner: &Rc<RefCell<Inner>>, message: ServerMessage) {
    match message {
        ServerMessage::Response {
            request_id,
            outcome,
        } => {
            let sender = inner.borrow_mut().pending.remove(&request_id);
            if let Some(sender) = sender {
                let result = match outcome {
                    ResponseOutcome::Ok { payload } => Ok(*payload),
                    ResponseOutcome::Error { error } => Err(error),
                };
                let _ = sender.send(result);
            }
        }
        ServerMessage::Event(event) => dispatch_event(inner, *event),
        ServerMessage::TaskStateChanged { .. } => {}
        ServerMessage::ShuttingDown => fail_connection(
            inner,
            "WakuWaku daemon is shutting down".to_owned(),
            Some((1000, "daemon shutting down")),
        ),
        ServerMessage::Hello { .. } | ServerMessage::Rejected { .. } => fail_connection(
            inner,
            "WakuWaku daemon sent an invalid post-handshake message".to_owned(),
            Some((1002, "invalid post-handshake message")),
        ),
    }
}

fn dispatch_event(inner: &Rc<RefCell<Inner>>, event: SequencedEvent) {
    let key = (event.session_id, event.runtime_id);
    let mut state = inner.borrow_mut();

    let is_new = match state.last_sequences.get_mut(&key) {
        Some(previous) if previous.epoch == event.epoch && event.sequence <= previous.sequence => {
            false
        }
        Some(previous) => {
            previous.epoch = event.epoch;
            previous.sequence = event.sequence;
            true
        }
        None => {
            state.last_sequences.insert(
                key,
                LastSequence {
                    epoch: event.epoch,
                    sequence: event.sequence,
                },
            );
            true
        }
    };
    if !is_new {
        return;
    }

    let has_live_subscriber = if let Some(subscribers) = state.subscribers.get_mut(&key) {
        subscribers.retain(|subscriber| subscriber.unbounded_send(event.clone()).is_ok());
        !subscribers.is_empty()
    } else {
        false
    };
    if has_live_subscriber {
        return;
    }
    state.subscribers.remove(&key);

    let buffered = state.buffered.entry(key).or_default();
    buffered.push_back(event);
    while buffered.len() > MAX_BUFFERED_EVENTS_PER_RUNTIME {
        buffered.pop_front();
    }
}

fn fail_connection(
    inner: &Rc<RefCell<Inner>>,
    message: String,
    close: Option<(u16, &'static str)>,
) {
    let socket = {
        let mut state = inner.borrow_mut();
        if state.closed {
            return;
        }
        state.closed = true;
        state.connected = false;

        if let Some(handshake) = state.handshake.take() {
            let _ = handshake.send(Err(message.clone()));
        }
        for sender in mem::take(&mut state.pending).into_values() {
            let _ = sender.send(Err(RpcError {
                message: message.clone(),
            }));
        }
        state.subscribers.clear();
        state.socket.clone()
    };

    if let Some((code, reason)) = close {
        let _ = socket.close_with_code_and_reason(code, reason);
    }
}

fn failed_request_task(
    foreground: ForegroundExecutor,
    message: String,
) -> Task<Result<ResponsePayload, RpcError>> {
    foreground.spawn(async move { Err(RpcError { message }) })
}

fn remove_pending(inner: &Rc<RefCell<Inner>>, request_id: Uuid) {
    inner.borrow_mut().pending.remove(&request_id);
}

fn page_is_https() -> bool {
    web_sys::window()
        .and_then(|window| window.location().protocol().ok())
        .is_some_and(|protocol| protocol.eq_ignore_ascii_case("https:"))
}

/// Normalize a daemon address to the authenticated `/v1` WebSocket endpoint.
fn daemon_url(address: &str) -> anyhow::Result<String> {
    let address = address.trim();
    if address.is_empty() {
        bail!("WakuWaku daemon address is required");
    }

    let lowercase = address.to_ascii_lowercase();
    let normalized = if lowercase.starts_with("ws://") || lowercase.starts_with("wss://") {
        address.to_owned()
    } else if lowercase.starts_with("http://") {
        format!("ws://{}", &address[7..])
    } else if lowercase.starts_with("https://") {
        format!("wss://{}", &address[8..])
    } else if address.contains("://") {
        bail!("WakuWaku daemon URL must use ws:// or wss://");
    } else if page_is_https() {
        bail!("This secure WakuWaku page requires an explicit wss:// daemon URL");
    } else {
        format!("ws://{address}")
    };

    let mut url = Url::parse(&normalized).context("WakuWaku daemon address is invalid")?;
    if !url.scheme().eq_ignore_ascii_case("ws") && !url.scheme().eq_ignore_ascii_case("wss") {
        bail!("WakuWaku daemon URL must use ws:// or wss://");
    }
    if page_is_https() && !url.scheme().eq_ignore_ascii_case("wss") {
        bail!("This secure WakuWaku page requires a wss:// daemon URL");
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        bail!("WakuWaku daemon URL must contain a host and no credentials");
    }

    url.set_path("/v1");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}
