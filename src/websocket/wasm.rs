//! WinterCG WebSocket implementation for WASM targets.
//!
//! This module provides WebSocket support for WinterCG-compatible runtimes
//! (like Cloudflare Workers) using the WebSocketPair API.

use crate::{
    header,
    websocket::{
        ffi,
        types::{WebSocketCloseFrame, WebSocketError, WebSocketResult},
    },
    Method, Request, Response, StatusCode,
};

pub use ffi::create_websocket_response;
use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures_core::Stream;
use http_kit::utils::ByteStr;
use http_kit::ws::{WebSocketConfig, WebSocketMessage};
use serde::Serialize;
use skyzen_core::{Extractor, Responder};
use std::{
    cell::RefCell,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};
use wasm_bindgen::{prelude::*, JsCast};

/// WebSocket connection for WASM targets.
///
/// # Platform Notes
/// - Maximum message size: 1 MiB (platform imposed)
/// - No ping/pong frame control (use `send_ping`/`send_pong` returns error)
/// - Event-driven model converted to Stream
pub struct WebSocket {
    inner: ffi::WebSocket,
    rx: UnboundedReceiver<WebSocketResult<WebSocketMessage>>,
    _closures: Rc<RefCell<EventClosures>>,
    config: WebSocketConfig,
}

/// Holds the event handler closures to prevent them from being dropped.
struct EventClosures {
    _on_message: Closure<dyn FnMut(ffi::MessageEvent)>,
    _on_close: Closure<dyn FnMut(ffi::CloseEvent)>,
    _on_error: Closure<dyn FnMut(ffi::ErrorEvent)>,
}

impl WebSocket {
    pub(crate) fn from_ffi_socket(socket: ffi::WebSocket, config: WebSocketConfig) -> Self {
        let (tx, rx) = mpsc::unbounded();

        // Create event handlers
        let closures = Self::setup_event_handlers(&socket, tx);

        Self {
            inner: socket,
            rx,
            _closures: Rc::new(RefCell::new(closures)),
            config,
        }
    }

    fn setup_event_handlers(
        socket: &ffi::WebSocket,
        tx: UnboundedSender<WebSocketResult<WebSocketMessage>>,
    ) -> EventClosures {
        // Message handler
        let tx_message = tx.clone();
        let on_message = Closure::wrap(Box::new(move |event: ffi::MessageEvent| {
            let data = event.data();

            let message = if let Some(text) = data.as_string() {
                WebSocketMessage::Text(text.into())
            } else if js_sys::Uint8Array::instanceof(&data) {
                let array = js_sys::Uint8Array::from(data);
                let mut bytes = vec![0u8; array.length() as usize];
                array.copy_to(&mut bytes);
                WebSocketMessage::Binary(bytes.into())
            } else {
                // Unknown data type, skip
                return;
            };

            let _ = tx_message.unbounded_send(Ok(message));
        }) as Box<dyn FnMut(ffi::MessageEvent)>);

        // Close handler
        let tx_close = tx.clone();
        let on_close = Closure::wrap(Box::new(move |_event: ffi::CloseEvent| {
            let _ = tx_close.unbounded_send(Ok(WebSocketMessage::Close));
        }) as Box<dyn FnMut(ffi::CloseEvent)>);

        // Error handler
        let on_error = Closure::wrap(Box::new(move |event: ffi::ErrorEvent| {
            let _ = tx.unbounded_send(Err(WebSocketError::Protocol(event.message())));
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
    #[cfg(feature = "json")]
    pub async fn send<T: Serialize>(&mut self, value: T) -> WebSocketResult<()> {
        let payload = serde_json::to_string(&value)?;
        self.send_text(payload).await
    }

    /// Send a raw text frame without JSON serialization.
    pub async fn send_text(&mut self, text: impl Into<ByteStr>) -> WebSocketResult<()> {
        let text = text.into();
        self.inner
            .send(&JsValue::from_str(&text))
            .map_err(|e| WebSocketError::Protocol(format!("{:?}", e)))
    }

    /// Send raw binary data without JSON serialization.
    pub async fn send_binary(&mut self, data: impl Into<Vec<u8>>) -> WebSocketResult<()> {
        let bytes = data.into();
        let array = js_sys::Uint8Array::from(&bytes[..]);
        self.inner
            .send(&array.into())
            .map_err(|e| WebSocketError::Protocol(format!("{:?}", e)))
    }

    /// Send a ping frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by WinterCG API)
    pub async fn send_ping(&mut self, _data: impl Into<Vec<u8>>) -> WebSocketResult<()> {
        Err(WebSocketError::Protocol(
            "Ping frames not supported on WASM platform".into(),
        ))
    }

    /// Send a pong frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by WinterCG API)
    pub async fn send_pong(&mut self, _data: impl Into<Vec<u8>>) -> WebSocketResult<()> {
        Err(WebSocketError::Protocol(
            "Pong frames not supported on WASM platform".into(),
        ))
    }

    /// Send a [`WebSocketMessage`] without additional processing.
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
    pub fn get_config(&self) -> &WebSocketConfig {
        &self.config
    }

    /// Close the websocket connection gracefully.
    pub async fn close(&mut self, close_frame: Option<WebSocketCloseFrame>) -> WebSocketResult<()> {
        if let Some(frame) = close_frame {
            self.inner.close(Some(frame.code), Some(&frame.reason));
        } else {
            self.inner.close(None, None);
        }
        Ok(())
    }

    /// Split the websocket into independent sender and receiver halves.
    ///
    /// # Note
    /// Splitting is not fully supported on WASM - both halves share the same underlying connection.
    /// This is provided for API compatibility but may have different semantics than native.
    pub fn split(self) -> (WebSocketSender, WebSocketReceiver) {
        let config = self.config.clone();
        let inner = self.inner;
        let closures = self._closures;

        (
            WebSocketSender {
                inner: inner.clone(),
                config: config.clone(),
                _closures: closures.clone(),
            },
            WebSocketReceiver {
                rx: self.rx,
                config,
                _closures: closures,
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
    #[cfg(feature = "json")]
    pub async fn send<T: Serialize>(&mut self, value: T) -> WebSocketResult<()> {
        let payload = serde_json::to_string(&value)?;
        self.send_text(payload).await
    }

    /// Send a raw text frame without JSON serialization.
    pub async fn send_text(&mut self, text: impl Into<ByteStr>) -> WebSocketResult<()> {
        let text = text.into();
        self.inner
            .send(&JsValue::from_str(&text))
            .map_err(|e| WebSocketError::Protocol(format!("{:?}", e)))
    }

    /// Send raw binary data without JSON serialization.
    pub async fn send_binary(&mut self, data: impl Into<Vec<u8>>) -> WebSocketResult<()> {
        let bytes = data.into();
        let array = js_sys::Uint8Array::from(&bytes[..]);
        self.inner
            .send(&array.into())
            .map_err(|e| WebSocketError::Protocol(format!("{:?}", e)))
    }

    /// Send a ping frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by WinterCG API)
    pub async fn send_ping(&mut self, _data: impl Into<Vec<u8>>) -> WebSocketResult<()> {
        Err(WebSocketError::Protocol(
            "Ping frames not supported on WASM platform".into(),
        ))
    }

    /// Send a pong frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by WinterCG API)
    pub async fn send_pong(&mut self, _data: impl Into<Vec<u8>>) -> WebSocketResult<()> {
        Err(WebSocketError::Protocol(
            "Pong frames not supported on WASM platform".into(),
        ))
    }

    /// Send a [`WebSocketMessage`] without additional processing.
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
    pub async fn close(&mut self, close_frame: Option<WebSocketCloseFrame>) -> WebSocketResult<()> {
        if let Some(frame) = close_frame {
            self.inner.close(Some(frame.code), Some(&frame.reason));
        } else {
            self.inner.close(None, None);
        }
        Ok(())
    }

    /// Access the underlying websocket configuration.
    pub fn get_config(&self) -> &WebSocketConfig {
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
    pub fn get_config(&self) -> &WebSocketConfig {
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
    pair: Option<SendSyncWebSocketPair>,
    protocols: Vec<String>,
    config: WebSocketConfig,
}

impl WebSocketUpgrade {
    fn new() -> Self {
        Self {
            pair: Some(SendSyncWebSocketPair(ffi::WebSocketPair::new())),
            protocols: Vec::new(),
            config: WebSocketConfig::default(),
        }
    }

    /// Negotiate the sub-protocol returned to the client.
    ///
    /// # Note
    /// On WASM, protocol negotiation is tracked but not enforced by the runtime.
    #[must_use]
    pub fn protocols<I, S>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.protocols = protocols
            .into_iter()
            .map(|s| s.as_ref().to_string())
            .collect();
        self
    }

    /// Override the [`WebSocketConfig`] used for the upgraded stream.
    #[must_use]
    pub const fn config(mut self, config: WebSocketConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the maximum incoming message size accepted by the websocket.
    ///
    /// # Platform Notes
    /// - **Native**: Enforced by async-tungstenite
    /// - **WASM**: 1 MiB limit enforced by runtime (this setting is advisory only)
    #[must_use]
    pub fn max_message_size(mut self, max_size: Option<usize>) -> Self {
        self.config.max_message_size = max_size;
        self
    }

    /// Finalize the handshake and start handling the upgraded socket with `callback`.
    pub fn on_upgrade<F, Fut>(mut self, callback: F) -> WebSocketUpgradeResponder
    where
        F: FnOnce(WebSocket) -> Fut + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        let pair = self.pair.take().expect("pair already consumed").0;
        let server = pair.server();
        let client = pair.client();

        // Accept the connection
        server.accept();

        // Create our WebSocket wrapper
        let socket = WebSocket::from_ffi_socket(server, self.config);

        // Spawn the callback to handle messages
        wasm_bindgen_futures::spawn_local(async move {
            callback(socket).await;
        });

        WebSocketUpgradeResponder {
            client: SendSyncWebSocket(client),
        }
    }
}

impl std::fmt::Debug for WebSocketUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketUpgrade")
            .field("protocols", &self.protocols)
            .field("config", &self.config)
            .finish()
    }
}

fn header_has_token(value: &header::HeaderValue, token: &str) -> bool {
    value
        .to_str()
        .map(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
        .unwrap_or(false)
}

impl Extractor for WebSocketUpgrade {
    type Error = WebSocketUpgradeError;

    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
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
            .map(|value| value.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
        {
            return Err(WebSocketUpgradeError::InvalidUpgradeHeader);
        }

        // Check version
        match headers.get(header::SEC_WEBSOCKET_VERSION) {
            Some(version) if version == "13" => {}
            _ => return Err(WebSocketUpgradeError::UnsupportedVersion),
        }

        Ok(WebSocketUpgrade::new())
    }
}

/// Wrapper to make `ffi::WebSocket` Send/Sync safe in single-threaded WASM environment.
///
/// This is used to store the client WebSocket in response extensions, which requires
/// `Send + Sync` bounds. The inner socket can be extracted via [`into_inner`](Self::into_inner).
#[derive(Clone)]
pub struct SendSyncWebSocket(pub(crate) ffi::WebSocket);

impl SendSyncWebSocket {
    /// Consume the wrapper and return the inner WebSocket.
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
}

impl std::fmt::Debug for WebSocketUpgradeResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketUpgradeResponder").finish()
    }
}

impl Responder for WebSocketUpgradeResponder {
    type Error = std::convert::Infallible;

    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        // Set status to 101 Switching Protocols
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;

        // Store the client socket in extensions for the runtime to extract
        // We use SendSyncWebSocket to satisfy Send + Sync bounds
        response.extensions_mut().insert(self.client);

        Ok(())
    }
}
