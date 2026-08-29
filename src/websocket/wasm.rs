//! `WinterCG` WebSocket implementation for WASM targets.
//!
//! This module provides WebSocket support for WinterCG-compatible runtimes
//! (like Cloudflare Workers) using the `WebSocketPair` API.

// Futures here hold JS handles, which are single-threaded by design. The host runtime's send and
// close calls are synchronous, so those methods return a ready future rather than an `async` block
// with nothing to await — call sites still `.await` them exactly as they do on the native backend.
#![allow(clippy::future_not_send)]

use crate::{
    header,
    websocket::{
        ffi,
        session::{internal_error_frame, IntoWebSocketOutcome},
        types::{
            offered_protocols, select_protocol, WebSocketCloseFrame, WebSocketError,
            WebSocketResult,
        },
    },
    Method, Request, Response, StatusCode,
};

use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures_core::Stream;
use http_kit::utils::ByteStr;
use http_kit::ws::{WebSocketConfig, WebSocketMessage};
use serde::Serialize;
use skyzen_core::{error::ErrorChain, Extractor, Responder};
use std::{
    cell::RefCell,
    future::{ready, Future},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};
use wasm_bindgen::{prelude::*, JsCast};

/// Reject an outbound message whose size exceeds the configured maximum, before handing it to the
/// host runtime (which would otherwise fail opaquely or truncate).
const fn ensure_within_limit(config: &WebSocketConfig, len: usize) -> WebSocketResult<()> {
    if let Some(limit) = config.max_message_size {
        if len > limit {
            return Err(WebSocketError::MessageTooLarge { len, limit });
        }
    }
    Ok(())
}

/// Ask the host runtime to close a socket, reporting a rejection instead of dropping it.
fn close_socket(
    socket: &ffi::WebSocket,
    close_frame: Option<WebSocketCloseFrame>,
) -> WebSocketResult<()> {
    let (code, reason) = close_frame.map_or((None, None), |frame| (Some(frame.code), Some(frame)));
    socket
        .close(code, reason.as_ref().map(|frame| frame.reason.as_str()))
        .map_err(|error| WebSocketError::Protocol(format!("{error:?}")))
}

/// WebSocket connection for WASM targets.
///
/// # Platform Notes
/// - Maximum message size: enforced from [`WebSocketConfig::max_message_size`]
/// - No ping/pong frame control (use `send_ping`/`send_pong` returns error)
/// - Event-driven model converted to Stream
pub struct WebSocket {
    inner: ffi::WebSocket,
    rx: UnboundedReceiver<WebSocketResult<WebSocketMessage>>,
    closures: Rc<RefCell<EventClosures>>,
    config: WebSocketConfig,
}

/// Holds the event handler closures to prevent them from being dropped.
#[allow(clippy::struct_field_names)]
struct EventClosures {
    _on_message: Closure<dyn FnMut(ffi::MessageEvent)>,
    _on_close: Closure<dyn FnMut(ffi::CloseEvent)>,
    _on_error: Closure<dyn FnMut(ffi::ErrorEvent)>,
}

impl WebSocket {
    pub(crate) fn from_ffi_socket(socket: ffi::WebSocket, config: WebSocketConfig) -> Self {
        let (tx, rx) = mpsc::unbounded();

        // Create event handlers
        let closures = Self::setup_event_handlers(&socket, tx, &config);

        Self {
            inner: socket,
            rx,
            closures: Rc::new(RefCell::new(closures)),
            config,
        }
    }

    fn setup_event_handlers(
        socket: &ffi::WebSocket,
        tx: UnboundedSender<WebSocketResult<WebSocketMessage>>,
        config: &WebSocketConfig,
    ) -> EventClosures {
        // Message handler
        let tx_message = tx.clone();
        let max_message_size = config.max_message_size;
        let on_message = Closure::wrap(Box::new(move |event: ffi::MessageEvent| {
            let data = event.data();

            let message = if let Some(text) = data.as_string() {
                WebSocketMessage::Text(text.into())
            } else if data.is_instance_of::<js_sys::ArrayBuffer>()
                || data.is_instance_of::<js_sys::Uint8Array>()
            {
                // WinterCG runtimes (e.g. Cloudflare Workers) deliver binary frames as
                // `ArrayBuffer`; `Uint8Array::new` handles both buffer and view inputs.
                WebSocketMessage::Binary(js_sys::Uint8Array::new(&data).to_vec().into())
            } else {
                // Unknown data type, skip
                return;
            };

            // Enforce the configured inbound size limit, matching native behavior where
            // async-tungstenite rejects oversized incoming messages.
            let len = match &message {
                WebSocketMessage::Text(text) => text.len(),
                WebSocketMessage::Binary(bytes) => bytes.len(),
                _ => 0,
            };
            if let Some(limit) = max_message_size {
                if len > limit {
                    let _ = tx_message
                        .unbounded_send(Err(WebSocketError::MessageTooLarge { len, limit }));
                    return;
                }
            }

            let _ = tx_message.unbounded_send(Ok(message));
        }) as Box<dyn FnMut(ffi::MessageEvent)>);

        // Close handler
        let tx_close = tx.clone();
        let on_close = Closure::wrap(Box::new(move |event: ffi::CloseEvent| {
            tracing::debug!(
                code = event.code(),
                reason = %event.reason(),
                was_clean = event.was_clean(),
                "websocket closed by peer"
            );
            let _ = tx_close.unbounded_send(Ok(WebSocketMessage::Close));
            // Terminate the stream so receive loops observe the end of the connection.
            tx_close.close_channel();
        }) as Box<dyn FnMut(ffi::CloseEvent)>);

        // Error handler
        let on_error = Closure::wrap(Box::new(move |event: ffi::ErrorEvent| {
            let _ = tx.unbounded_send(Err(WebSocketError::Protocol(event.message())));
            // Terminate the stream so receive loops observe the failure and end.
            tx.close_channel();
        }) as Box<dyn FnMut(ffi::ErrorEvent)>);

        // Attach event listeners
        socket.add_event_listener("message", on_message.as_ref().unchecked_ref());
        socket.add_event_listener("close", on_close.as_ref().unchecked_ref());
        socket.add_event_listener("error", on_error.as_ref().unchecked_ref());

        EventClosures {
            _on_message: on_message,
            _on_close: on_close,
            _on_error: on_error,
        }
    }

    /// Serialize a value to JSON text and send it over the websocket connection.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError`] if serialization fails, the message exceeds
    /// the configured size limit, or the runtime rejects the send.
    #[cfg(feature = "json")]
    pub async fn send<T: Serialize>(&mut self, value: T) -> WebSocketResult<()> {
        let payload = serde_json::to_string(&value)?;
        self.send_text(payload).await
    }

    /// Send a raw text frame without JSON serialization.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError`] if the message exceeds the configured size
    /// limit or the runtime rejects the send.
    pub fn send_text(
        &mut self,
        text: impl Into<ByteStr>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        let text = text.into();
        ready(
            ensure_within_limit(&self.config, text.len()).and_then(|()| {
                self.inner
                    .send(&JsValue::from_str(&text))
                    .map_err(|e| WebSocketError::Protocol(format!("{e:?}")))
            }),
        )
    }

    /// Send raw binary data without JSON serialization.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError`] if the message exceeds the configured size
    /// limit or the runtime rejects the send.
    pub fn send_binary(
        &mut self,
        data: impl Into<Vec<u8>>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        let bytes = data.into();
        ready(
            ensure_within_limit(&self.config, bytes.len()).and_then(|()| {
                let array = js_sys::Uint8Array::from(&bytes[..]);
                self.inner
                    .send(&array.into())
                    .map_err(|e| WebSocketError::Protocol(format!("{e:?}")))
            }),
        )
    }

    /// Send a ping frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by `WinterCG` API)
    ///
    /// # Errors
    ///
    /// Always returns [`WebSocketError::Protocol`] on WASM.
    pub fn send_ping(
        &mut self,
        _data: impl Into<Vec<u8>>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        ready(Err(WebSocketError::Protocol(
            "Ping frames not supported on WASM platform".into(),
        )))
    }

    /// Send a pong frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by `WinterCG` API)
    ///
    /// # Errors
    ///
    /// Always returns [`WebSocketError::Protocol`] on WASM.
    pub fn send_pong(
        &mut self,
        _data: impl Into<Vec<u8>>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        ready(Err(WebSocketError::Protocol(
            "Pong frames not supported on WASM platform".into(),
        )))
    }

    /// Send a [`WebSocketMessage`] without additional processing.
    ///
    /// # Errors
    ///
    /// Propagates the error from the underlying send or close operation.
    pub async fn send_message(&mut self, message: WebSocketMessage) -> WebSocketResult<()> {
        match message {
            WebSocketMessage::Text(text) => self.send_text(text).await,
            WebSocketMessage::Binary(data) => self.send_binary(data).await,
            WebSocketMessage::Close => self.close(None).await,
            WebSocketMessage::Ping(_) => self.send_ping(vec![]).await,
            WebSocketMessage::Pong(_) => self.send_pong(vec![]).await,
        }
    }

    /// Receive and deserialize the next JSON message.
    ///
    /// Skips non-text messages and returns None when connection closes.
    #[cfg(feature = "json")]
    pub async fn recv_json<T: serde::de::DeserializeOwned>(
        &mut self,
    ) -> Option<WebSocketResult<T>> {
        use futures_util::StreamExt;

        loop {
            match self.next().await {
                Some(Ok(msg)) => {
                    if let Some(result) = msg.into_json() {
                        return Some(result.map_err(WebSocketError::from));
                    }
                    // Skip non-text messages, continue loop
                }
                Some(Err(e)) => return Some(Err(e)),
                None => return None,
            }
        }
    }

    /// Access the underlying websocket configuration.
    #[must_use]
    pub const fn get_config(&self) -> &WebSocketConfig {
        &self.config
    }

    /// Close the websocket connection gracefully.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the host runtime rejects the close — most often a
    /// close code it does not allow the caller to send.
    pub fn close(
        &mut self,
        close_frame: Option<WebSocketCloseFrame>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        ready(close_socket(&self.inner, close_frame))
    }

    /// Split the websocket into independent sender and receiver halves.
    ///
    /// # Note
    /// Splitting is not fully supported on WASM - both halves share the same underlying connection.
    /// This is provided for API compatibility but may have different semantics than native.
    #[must_use]
    pub fn split(self) -> (WebSocketSender, WebSocketReceiver) {
        (
            WebSocketSender {
                inner: self.inner,
                config: self.config.clone(),
                _closures: self.closures.clone(),
            },
            WebSocketReceiver {
                rx: self.rx,
                config: self.config,
                _closures: self.closures,
            },
        )
    }
}

impl std::fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocket").finish_non_exhaustive()
    }
}

impl Stream for WebSocket {
    type Item = WebSocketResult<WebSocketMessage>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// Sender half returned from [`WebSocket::split`].
///
/// # Note
/// On WASM, this shares the underlying connection with the receiver.
pub struct WebSocketSender {
    inner: ffi::WebSocket,
    config: WebSocketConfig,
    _closures: Rc<RefCell<EventClosures>>,
}

impl WebSocketSender {
    /// Serialize a value to JSON text and send it over the websocket connection.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError`] if serialization fails, the message exceeds
    /// the configured size limit, or the runtime rejects the send.
    #[cfg(feature = "json")]
    pub async fn send<T: Serialize>(&mut self, value: T) -> WebSocketResult<()> {
        let payload = serde_json::to_string(&value)?;
        self.send_text(payload).await
    }

    /// Send a raw text frame without JSON serialization.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError`] if the message exceeds the configured size
    /// limit or the runtime rejects the send.
    pub fn send_text(
        &mut self,
        text: impl Into<ByteStr>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        let text = text.into();
        ready(
            ensure_within_limit(&self.config, text.len()).and_then(|()| {
                self.inner
                    .send(&JsValue::from_str(&text))
                    .map_err(|e| WebSocketError::Protocol(format!("{e:?}")))
            }),
        )
    }

    /// Send raw binary data without JSON serialization.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError`] if the message exceeds the configured size
    /// limit or the runtime rejects the send.
    pub fn send_binary(
        &mut self,
        data: impl Into<Vec<u8>>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        let bytes = data.into();
        ready(
            ensure_within_limit(&self.config, bytes.len()).and_then(|()| {
                let array = js_sys::Uint8Array::from(&bytes[..]);
                self.inner
                    .send(&array.into())
                    .map_err(|e| WebSocketError::Protocol(format!("{e:?}")))
            }),
        )
    }

    /// Send a ping frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by `WinterCG` API)
    ///
    /// # Errors
    ///
    /// Always returns [`WebSocketError::Protocol`] on WASM.
    pub fn send_ping(
        &mut self,
        _data: impl Into<Vec<u8>>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        ready(Err(WebSocketError::Protocol(
            "Ping frames not supported on WASM platform".into(),
        )))
    }

    /// Send a pong frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by `WinterCG` API)
    ///
    /// # Errors
    ///
    /// Always returns [`WebSocketError::Protocol`] on WASM.
    pub fn send_pong(
        &mut self,
        _data: impl Into<Vec<u8>>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        ready(Err(WebSocketError::Protocol(
            "Pong frames not supported on WASM platform".into(),
        )))
    }

    /// Send a [`WebSocketMessage`] without additional processing.
    ///
    /// # Errors
    ///
    /// Propagates the error from the underlying send or close operation.
    pub async fn send_message(&mut self, message: WebSocketMessage) -> WebSocketResult<()> {
        match message {
            WebSocketMessage::Text(text) => self.send_text(text).await,
            WebSocketMessage::Binary(data) => self.send_binary(data).await,
            WebSocketMessage::Close => self.close(None).await,
            WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) => Err(WebSocketError::Protocol(
                "Ping/Pong not supported on WASM".into(),
            )),
        }
    }

    /// Close the websocket connection gracefully.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the host runtime rejects the close — most often a
    /// close code it does not allow the caller to send.
    pub fn close(
        &mut self,
        close_frame: Option<WebSocketCloseFrame>,
    ) -> impl Future<Output = WebSocketResult<()>> {
        ready(close_socket(&self.inner, close_frame))
    }

    /// Access the underlying websocket configuration.
    #[must_use]
    pub const fn get_config(&self) -> &WebSocketConfig {
        &self.config
    }
}

impl std::fmt::Debug for WebSocketSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketSender").finish_non_exhaustive()
    }
}

/// Receiver half returned from [`WebSocket::split`].
pub struct WebSocketReceiver {
    rx: UnboundedReceiver<WebSocketResult<WebSocketMessage>>,
    config: WebSocketConfig,
    _closures: Rc<RefCell<EventClosures>>,
}

impl WebSocketReceiver {
    /// Receive and deserialize the next JSON message.
    ///
    /// Skips non-text messages and returns None when connection closes.
    #[cfg(feature = "json")]
    pub async fn recv_json<T: serde::de::DeserializeOwned>(
        &mut self,
    ) -> Option<WebSocketResult<T>> {
        use futures_util::StreamExt;

        loop {
            match self.next().await {
                Some(Ok(msg)) => {
                    if let Some(result) = msg.into_json() {
                        return Some(result.map_err(WebSocketError::from));
                    }
                    // Skip non-text messages, continue loop
                }
                Some(Err(e)) => return Some(Err(e)),
                None => return None,
            }
        }
    }

    /// Access the underlying websocket configuration.
    #[must_use]
    pub const fn get_config(&self) -> &WebSocketConfig {
        &self.config
    }
}

impl std::fmt::Debug for WebSocketReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketReceiver").finish_non_exhaustive()
    }
}

impl Stream for WebSocketReceiver {
    type Item = WebSocketResult<WebSocketMessage>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// Errors that can occur during WebSocket upgrade.
#[skyzen::error(status = StatusCode::BAD_REQUEST)]
pub enum WebSocketUpgradeError {
    /// The HTTP method is not GET.
    #[error("Method not allowed", status = StatusCode::METHOD_NOT_ALLOWED)]
    MethodNotAllowed,

    /// The `Upgrade` header is missing or invalid.
    #[error("Missing or invalid upgrade header")]
    MissingUpgradeHeader,

    /// The `Connection` header is missing.
    #[error("Missing Connection header for WebSocket request")]
    MissingConnectionHeader,

    /// The `Sec-WebSocket-Key` header is missing.
    #[error("Missing Sec-WebSocket-Key header")]
    MissingSecWebSocketKey,

    /// The `Upgrade` header is not `websocket`.
    #[error("Upgrade header must be `websocket`")]
    InvalidUpgradeHeader,

    /// The `Connection` header is invalid.
    #[error("Invalid Connection header for WebSocket request")]
    InvalidConnectionHeader,

    /// The `Sec-WebSocket-Version` header is not `13`.
    #[error("Unsupported Sec-WebSocket-Version. Only version 13 is accepted")]
    UnsupportedVersion,
}

/// Wrapper to make `WebSocketPair` Send/Sync safe in single-threaded WASM environment.
struct SendSyncWebSocketPair(ffi::WebSocketPair);

// SAFETY: WASM is single-threaded, so Send/Sync is safe for JsValue wrappers.
unsafe impl Send for SendSyncWebSocketPair {}
unsafe impl Sync for SendSyncWebSocketPair {}

/// Helper that contains the state required to accept a WebSocket connection.
pub struct WebSocketUpgrade {
    pair: SendSyncWebSocketPair,
    requested_protocols: Vec<String>,
    response_protocol: Option<header::HeaderValue>,
    config: WebSocketConfig,
}

impl WebSocketUpgrade {
    fn new(requested_protocols: Vec<String>) -> Self {
        Self {
            pair: SendSyncWebSocketPair(ffi::WebSocketPair::new()),
            requested_protocols,
            response_protocol: None,
            config: WebSocketConfig::default(),
        }
    }

    /// Answer the handshake with the first of `protocols` the client also offered.
    ///
    /// The chosen one is echoed in the `101`'s `Sec-WebSocket-Protocol`, which RFC 6455 §4.1
    /// requires whenever the client offered any: without it the client fails the connection.
    #[must_use]
    pub fn protocols<I, S>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let supported: Vec<String> = protocols
            .into_iter()
            .map(|protocol| protocol.as_ref().to_owned())
            .collect();

        self.response_protocol = select_protocol(&self.requested_protocols, &supported);
        self
    }

    /// Answer the handshake with exactly this subprotocol, whatever the client offered.
    ///
    /// [`protocols`](Self::protocols) covers the case where the server knows the whole set of
    /// acceptable names up front. It cannot cover the one where the *value* carries information —
    /// a browser cannot attach an `Authorization` header to a `WebSocket`, so the standard way to
    /// authenticate one is to smuggle the credential through the subprotocol list
    /// (`new WebSocket(url, ["app.bearer." + token])`) and have the server echo the token back.
    /// Read the offer with [`requested_protocols`](Self::requested_protocols), then answer with it.
    ///
    /// A [`HeaderValue`](header::HeaderValue) rather than a string, because a subprotocol that
    /// cannot be sent as a header value is not an answer — and the handler, not the handshake, is
    /// where that is known.
    #[must_use]
    pub fn protocol(mut self, protocol: header::HeaderValue) -> Self {
        self.response_protocol = Some(protocol);
        self
    }

    /// The subprotocols the client offered, in the order it offered them.
    #[must_use]
    pub fn requested_protocols(&self) -> &[String] {
        &self.requested_protocols
    }

    /// Override the [`WebSocketConfig`] used for the upgraded stream.
    #[must_use]
    pub const fn config(mut self, config: WebSocketConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the maximum message size accepted/sent by the websocket.
    ///
    /// # Platform Notes
    /// - **Native**: Enforced by async-tungstenite on both directions.
    /// - **WASM**: Enforced by Skyzen on both outbound sends and inbound messages (oversized
    ///   inbound messages surface as [`WebSocketError::MessageTooLarge`] on the receive stream).
    ///   Note the host runtime may impose its own lower cap (e.g. Cloudflare Workers limits
    ///   messages to 1 MiB), so set this accordingly.
    #[must_use]
    pub const fn max_message_size(mut self, max_size: Option<usize>) -> Self {
        self.config.max_message_size = max_size;
        self
    }

    /// Finalize the handshake and start handling the upgraded socket with `callback`.
    ///
    /// The callback owns the connection until it returns. What it returns says how the session
    /// ended: `()` says nothing beyond "it ended", while a `Result<(), E>` — for any `E` that
    /// converts into [`Error`](skyzen_core::error::Error), which includes [`WebSocketError`] —
    /// lets the handler use `?` and report what stopped it. An error is logged with its whole
    /// `source()` chain, and the framework then closes the connection with
    /// [`INTERNAL_ERROR`](super::INTERNAL_ERROR).
    pub fn on_upgrade<F, Fut, R>(self, callback: F) -> WebSocketUpgradeResponder
    where
        F: FnOnce(WebSocket) -> Fut + 'static,
        Fut: std::future::Future<Output = R> + 'static,
        R: IntoWebSocketOutcome + 'static,
    {
        let Self {
            pair,
            response_protocol,
            config,
            requested_protocols: _,
        } = self;
        let pair = pair.0;
        let server = pair.server();
        let client = pair.client();

        // Accept the connection
        server.accept();

        // A JS websocket is a handle, so the framework keeps one of its own to close with. The
        // native backend has to work harder for the same thing: see `SessionStream` there.
        let closer = server.clone();

        // Create our WebSocket wrapper
        let socket = WebSocket::from_ffi_socket(server, config);

        // Spawn the callback to handle messages
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = callback(socket).await.into_outcome() {
                tracing::error!(error = %ErrorChain(&error), "websocket session handler failed");
                // Best-effort, exactly as on the native backend: the session already failed, and a
                // runtime that refuses the close leaves nothing further to do but say so.
                if let Err(error) = close_socket(&closer, Some(internal_error_frame())) {
                    tracing::debug!("failed to close a failed websocket session: {error}");
                }
            }
        });

        WebSocketUpgradeResponder {
            client: SendSyncWebSocket(client),
            response_protocol,
        }
    }
}

impl std::fmt::Debug for WebSocketUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketUpgrade")
            .field("requested_protocols", &self.requested_protocols)
            .field("response_protocol", &self.response_protocol)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn header_has_token(value: &header::HeaderValue, token: &str) -> bool {
    value.to_str().is_ok_and(|value| {
        value
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case(token))
    })
}

impl Extractor for WebSocketUpgrade {
    type Error = WebSocketUpgradeError;

    // The handshake is validated from the request's own headers, so the future is ready on
    // creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(validate_upgrade(request))
    }
}

/// Check the handshake headers of an incoming upgrade request.
fn validate_upgrade(request: &Request) -> Result<WebSocketUpgrade, WebSocketUpgradeError> {
    // Validate WebSocket upgrade request
    if request.method() != Method::GET {
        return Err(WebSocketUpgradeError::MethodNotAllowed);
    }

    let headers = request.headers();

    // Check Sec-WebSocket-Key
    headers
        .get(header::SEC_WEBSOCKET_KEY)
        .ok_or(WebSocketUpgradeError::MissingSecWebSocketKey)?;

    // Check Connection header
    let connection = headers
        .get(header::CONNECTION)
        .ok_or(WebSocketUpgradeError::MissingConnectionHeader)?;

    if !header_has_token(connection, "upgrade") {
        return Err(WebSocketUpgradeError::InvalidConnectionHeader);
    }

    // Check Upgrade header
    let upgrade_header = headers
        .get(header::UPGRADE)
        .ok_or(WebSocketUpgradeError::MissingUpgradeHeader)?;

    if !upgrade_header
        .to_str()
        .is_ok_and(|value| value.eq_ignore_ascii_case("websocket"))
    {
        return Err(WebSocketUpgradeError::InvalidUpgradeHeader);
    }

    // Check version
    match headers.get(header::SEC_WEBSOCKET_VERSION) {
        Some(version) if version == "13" => {}
        _ => return Err(WebSocketUpgradeError::UnsupportedVersion),
    }

    Ok(WebSocketUpgrade::new(offered_protocols(headers)))
}

/// Wrapper to make `ffi::WebSocket` Send/Sync safe in single-threaded WASM environment.
///
/// This is used to store the client WebSocket in response extensions, which requires
/// `Send + Sync` bounds. The inner socket can be extracted via [`into_inner`](Self::into_inner).
#[derive(Clone)]
pub struct SendSyncWebSocket(pub(crate) ffi::WebSocket);

impl SendSyncWebSocket {
    /// Consume the wrapper and return the inner WebSocket.
    #[must_use]
    pub fn into_inner(self) -> ffi::WebSocket {
        self.0
    }
}

impl std::fmt::Debug for SendSyncWebSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendSyncWebSocket").finish_non_exhaustive()
    }
}

// SAFETY: WASM is single-threaded, so Send/Sync is safe for JsValue wrappers.
unsafe impl Send for SendSyncWebSocket {}
unsafe impl Sync for SendSyncWebSocket {}

/// [`Responder`] returned from [`WebSocketUpgrade::on_upgrade`].
pub struct WebSocketUpgradeResponder {
    client: SendSyncWebSocket,
    response_protocol: Option<header::HeaderValue>,
}

impl std::fmt::Debug for WebSocketUpgradeResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketUpgradeResponder")
            .field("response_protocol", &self.response_protocol)
            .finish_non_exhaustive()
    }
}

impl Responder for WebSocketUpgradeResponder {
    type Error = std::convert::Infallible;

    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        // Set status to 101 Switching Protocols
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;

        // The answer to the client's subprotocol offer is a response header like any other; the
        // runtime carries the whole header map onto the `101` it builds for the host.
        if let Some(protocol) = self.response_protocol {
            response
                .headers_mut()
                .insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
        }

        // Store the client socket in extensions for the runtime to extract
        // We use SendSyncWebSocket to satisfy Send + Sync bounds
        response.extensions_mut().insert(self.client);

        Ok(())
    }
}
