//! Native (tokio/hyper) WebSocket upgrader and helpers.
//!
//! Handlers can request a protocol switch by extracting [`WebSocketUpgrade`]
//! and returning the result of [`WebSocketUpgrade::on_upgrade`]:
//! ```
//! use futures_util::StreamExt;
//! use skyzen::{websocket::{WebSocketMessage, WebSocketUpgrade}, Responder};
//!
//! async fn ws_handler(ws: WebSocketUpgrade) -> impl Responder {
//!     ws.on_upgrade(|mut socket| async move {
//!         while let Some(Ok(message)) = socket.next().await {
//!             if let Some(reply) = message.into_text() {
//!                 let _ = socket.send_text(reply).await;
//!             }
//!         }
//!     })
//! }
//! ```

use crate::runtime::native::Spawner;
use crate::{
    header,
    websocket::types::{WebSocketCloseFrame, WebSocketError, WebSocketResult},
    Method, Request, Response, StatusCode,
};
use async_tungstenite::{
    tokio::TokioAdapter,
    tungstenite::{
        protocol::{
            frame::{coding::CloseCode, Utf8Bytes},
            CloseFrame as TungsteniteCloseFrame, Role, WebSocketConfig as TungsteniteConfig,
        },
        Error as TungsteniteError, Message as TungsteniteMessage,
    },
    WebSocketReceiver as AsyncWebSocketReceiver, WebSocketSender as AsyncWebSocketSender,
    WebSocketStream,
};
use futures_core::Stream;
use futures_util::Sink;
use http_kit::{
    utils::{ByteStr, Bytes},
    ws::{WebSocketConfig, WebSocketMessage},
};
use hyper::{
    rt::{Read, ReadBuf, Write},
    upgrade::{OnUpgrade, Upgraded},
};
use serde::Serialize;
use skyzen_core::{Extractor, Responder};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{
    AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf as TokioReadBuf,
};
use tracing::error;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Errors that can occur during WebSocket upgrade.
#[skyzen::error(status = StatusCode::BAD_REQUEST)]
pub enum WebSocketUpgradeError {
    /// The HTTP method is not GET.
    #[error("Method not allowed", status = StatusCode::METHOD_NOT_ALLOWED)]
    MethodNotAllowed,

    /// The `Upgrade` header is missing.
    #[error("Missing upgrade header")]
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
    /// The `OnUpgrade` extension is missing.
    #[error("Missing OnUpgrade extension", status = StatusCode::UPGRADE_REQUIRED)]
    MissingOnUpgrade,
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

fn parse_protocols(value: Option<&header::HeaderValue>) -> Vec<String> {
    value
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn compute_accept_header(key: &header::HeaderValue) -> header::HeaderValue {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(GUID.as_bytes());
    let digest = hasher.finalize();
    let encoded = STANDARD.encode(digest);
    header::HeaderValue::from_str(&encoded).expect("Fail to create Sec-WebSocket-Accept header")
}

/// Upgraded
#[derive(Debug)]
pub struct UpgradedIo(Upgraded);

impl TokioAsyncRead for UpgradedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut TokioReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        let mut hyper_buf = ReadBuf::uninit(unsafe { buf.unfilled_mut() });
        let cursor = hyper_buf.unfilled();
        match Pin::new(&mut this.0).poll_read(cx, cursor) {
            Poll::Ready(Ok(())) => {
                let filled = hyper_buf.filled().len();
                buf.advance(filled);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl TokioAsyncWrite for UpgradedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.get_mut().0).poll_write_vectored(cx, bufs)
    }
}

type NativeIo = TokioAdapter<UpgradedIo>;

/// Stream representing a WebSocket connection handled by `async-tungstenite`.
pub struct WebSocket {
    inner: WebSocketStream<NativeIo>,
    config: WebSocketConfig,
}

impl WebSocket {
    pub(crate) async fn from_raw_socket(
        stream: NativeIo,
        role: Role,
        config: WebSocketConfig,
    ) -> Self {
        let inner =
            WebSocketStream::from_raw_socket(stream, role, Some(to_tungstenite_config(&config)))
                .await;
        Self { inner, config }
    }

    /// Serialize a value to JSON text and send it over the websocket connection.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the serialization fails.
    pub async fn send<T: Serialize>(&mut self, value: T) -> WebSocketResult<()> {
        let payload = serde_json::to_string(&value)?;
        self.send_text(payload).await
    }

    /// Send a raw text frame without JSON serialization.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the message is not text.
    pub async fn send_text(&mut self, text: impl Into<ByteStr>) -> WebSocketResult<()> {
        self.send_message(WebSocketMessage::text(text)).await
    }

    /// Send raw binary data without JSON serialization.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the message is not binary.
    pub async fn send_binary(&mut self, data: impl Into<Bytes>) -> WebSocketResult<()> {
        self.send_message(WebSocketMessage::binary(data)).await
    }

    /// Send a ping frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by `WinterCG` API)
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the message is not ping.
    pub async fn send_ping(&mut self, data: impl Into<Bytes>) -> WebSocketResult<()> {
        self.send_message(WebSocketMessage::Ping(data.into())).await
    }

    /// Send a pong frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by `WinterCG` API)
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the message is not pong.
    pub async fn send_pong(&mut self, data: impl Into<Bytes>) -> WebSocketResult<()> {
        self.send_message(WebSocketMessage::Pong(data.into())).await
    }

    /// Send a [`WebSocketMessage`] without additional processing.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Transport`] if the connection fails to send the message.
    pub async fn send_message(&mut self, message: WebSocketMessage) -> WebSocketResult<()> {
        self.inner
            .send(to_tungstenite_msg(message))
            .await
            .map_err(WebSocketError::from)
    }

    /// Receive and deserialize the next JSON message.
    ///
    /// Skips non-text messages and returns None when connection closes.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use skyzen::websocket::WebSocket;
    /// # use serde::Deserialize;
    /// # #[derive(Deserialize)]
    /// # struct MyData { value: i32 }
    /// # async fn example(mut socket: WebSocket) {
    /// while let Some(Ok(data)) = socket.recv_json::<MyData>().await {
    ///     println!("Received: {}", data.value);
    /// }
    /// # }
    /// ```
    #[cfg(feature = "json")]
    pub async fn recv_json<T: serde::de::DeserializeOwned>(
        &mut self,
    ) -> Option<WebSocketResult<T>> {
        use futures_util::StreamExt;

        loop {
            match self.next().await {
                Some(Ok(msg)) => {
                    if let Some(result) = msg.into_json::<T>() {
                        return result.map_err(WebSocketError::from).into();
                    }
                    // Skip non-text messages, continue loop
                }
                Some(Err(e)) => return Some(Err(e)),
                None => return None,
            }
        }
    }

    /// Access the underlying websocket configuration.
    pub const fn get_config(&self) -> &WebSocketConfig {
        &self.config
    }

    /// Close the websocket connection gracefully.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Transport`] if the connection fails to close.
    pub async fn close(&mut self, close_frame: Option<WebSocketCloseFrame>) -> WebSocketResult<()> {
        self.inner
            .close(close_frame.map(Into::into))
            .await
            .map_err(WebSocketError::from)
    }

    /// Split the websocket into independent sender and receiver halves.
    pub fn split(self) -> (WebSocketSender, WebSocketReceiver) {
        let config = self.config.clone();
        let (inner_sink, inner_stream) = self.inner.split();

        (
            WebSocketSender {
                inner: inner_sink,
                config: config.clone(),
            },
            WebSocketReceiver {
                inner: inner_stream,
                config,
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
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(message))) => Poll::Ready(Some(Ok(to_websocket_msg(message)))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Sink<WebSocketMessage> for WebSocket {
    type Error = WebSocketError;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_ready(cx)
            .map_err(WebSocketError::from)
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        item: WebSocketMessage,
    ) -> std::result::Result<(), Self::Error> {
        Pin::new(&mut self.inner)
            .start_send(to_tungstenite_msg(item))
            .map_err(WebSocketError::from)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_flush(cx)
            .map_err(WebSocketError::from)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_close(cx)
            .map_err(WebSocketError::from)
    }
}

/// Sender half returned from [`WebSocket::split`].
pub struct WebSocketSender {
    inner: AsyncWebSocketSender<NativeIo>,
    config: WebSocketConfig,
}

impl WebSocketSender {
    /// Serialize a value to JSON text and send it over the websocket connection.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the serialization fails.
    pub async fn send<T: Serialize>(&mut self, value: T) -> WebSocketResult<()> {
        let payload = serde_json::to_string(&value)?;
        self.send_text(payload).await
    }

    /// Send a raw text frame without JSON serialization.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the message is not text.
    pub async fn send_text(&mut self, text: impl Into<ByteStr>) -> WebSocketResult<()> {
        self.send_message(WebSocketMessage::text(text)).await
    }

    /// Send raw binary data without JSON serialization.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the message is not binary.
    pub async fn send_binary(&mut self, data: impl Into<Bytes>) -> WebSocketResult<()> {
        self.send_message(WebSocketMessage::binary(data)).await
    }

    /// Send a ping frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by `WinterCG` API)
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the message is not ping.
    pub async fn send_ping(&mut self, data: impl Into<Bytes>) -> WebSocketResult<()> {
        self.send_message(WebSocketMessage::Ping(data.into())).await
    }

    /// Send a pong frame with optional payload.
    ///
    /// # Platform Notes
    /// - **Native**: Full support
    /// - **WASM**: Returns error (not supported by `WinterCG` API)
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Protocol`] if the message is not pong.
    pub async fn send_pong(&mut self, data: impl Into<Bytes>) -> WebSocketResult<()> {
        self.send_message(WebSocketMessage::Pong(data.into())).await
    }

    /// Send a [`WebSocketMessage`] without additional processing.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Transport`] if the connection fails to send the message.
    pub async fn send_message(&mut self, message: WebSocketMessage) -> WebSocketResult<()> {
        self.inner
            .send(to_tungstenite_msg(message))
            .await
            .map_err(WebSocketError::from)
    }

    /// Close the websocket connection gracefully.
    ///
    /// # Errors
    ///
    /// Returns [`WebSocketError::Transport`] if the connection fails to close.
    pub async fn close(&mut self, close_frame: Option<WebSocketCloseFrame>) -> WebSocketResult<()> {
        self.inner
            .close(close_frame.map(Into::into))
            .await
            .map_err(WebSocketError::from)
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

impl Sink<WebSocketMessage> for WebSocketSender {
    type Error = WebSocketError;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_ready(cx)
            .map_err(WebSocketError::from)
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        item: WebSocketMessage,
    ) -> std::result::Result<(), Self::Error> {
        Pin::new(&mut self.inner)
            .start_send(to_tungstenite_msg(item))
            .map_err(WebSocketError::from)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_flush(cx)
            .map_err(WebSocketError::from)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_close(cx)
            .map_err(WebSocketError::from)
    }
}

/// Receiver half returned from [`WebSocket::split`].
pub struct WebSocketReceiver {
    inner: AsyncWebSocketReceiver<NativeIo>,
    config: WebSocketConfig,
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
                        return result.map_err(WebSocketError::from).into();
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
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(message))) => Poll::Ready(Some(Ok(to_websocket_msg(message)))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Helper that contains the state required to accept a WebSocket connection.
pub struct WebSocketUpgrade {
    key: header::HeaderValue,
    on_upgrade: OnUpgrade,
    requested_protocols: Vec<String>,
    response_protocol: Option<String>,
    config: WebSocketConfig,
    spawner: Option<Spawner>,
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

impl WebSocketUpgrade {
    /// Negotiate the sub-protocol returned to the client.
    #[must_use]
    pub fn protocols<I, S>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let supported: Vec<String> = protocols
            .into_iter()
            .map(|protocol| protocol.as_ref().to_string())
            .collect();

        self.response_protocol = self.requested_protocols.iter().find_map(|requested| {
            supported
                .iter()
                .find(|supported| *supported == requested)
                .cloned()
                .map(|_| requested.clone())
        });
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
    /// Pass `None` to disable the limit enforced by the backend implementation.
    #[must_use]
    pub const fn max_message_size(mut self, max_size: Option<usize>) -> Self {
        self.config.max_message_size = max_size;
        self
    }

    /// Finalize the handshake and start handling the upgraded socket with `callback`.
    pub fn on_upgrade<F, Fut>(self, callback: F) -> WebSocketUpgradeResponder
    where
        F: FnOnce(WebSocket) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        WebSocketUpgradeResponder {
            upgrade: self,
            callback: Some(Box::new(move |socket| {
                Box::pin(callback(socket)) as WebSocketCallbackFuture
            })),
        }
    }
}

type WebSocketCallbackFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
type DynCallback = Box<dyn FnOnce(WebSocket) -> WebSocketCallbackFuture + Send + Sync>;

fn upgrade(request: &mut Request) -> Result<WebSocketUpgrade, WebSocketUpgradeError> {
    if request.method() != Method::GET {
        return Err(WebSocketUpgradeError::MethodNotAllowed);
    }
    let (key, requested_protocols) = {
        let headers = request.headers();

        let key = headers
            .get(header::SEC_WEBSOCKET_KEY)
            .ok_or(WebSocketUpgradeError::MissingSecWebSocketKey)?
            .clone();

        let connection = headers
            .get(header::CONNECTION)
            .ok_or(WebSocketUpgradeError::MissingConnectionHeader)?;

        if !header_has_token(connection, "upgrade") {
            return Err(WebSocketUpgradeError::MissingUpgradeHeader);
        }

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

        match headers.get(header::SEC_WEBSOCKET_VERSION) {
            Some(version) if version == "13" => {}
            _ => {
                return Err(WebSocketUpgradeError::UnsupportedVersion);
            }
        }

        let requested_protocols = parse_protocols(headers.get(header::SEC_WEBSOCKET_PROTOCOL));

        (key, requested_protocols)
    };

    let on_upgrade = request
        .extensions_mut()
        .remove::<OnUpgrade>()
        .ok_or(WebSocketUpgradeError::MissingOnUpgrade)?;

    // Extract spawner from request extensions (injected by the runtime)
    let spawner = request.extensions_mut().remove::<Spawner>();

    Ok(WebSocketUpgrade {
        key,
        on_upgrade,
        requested_protocols,
        response_protocol: None,
        config: WebSocketConfig::default(),
        spawner,
    })
}

impl Extractor for WebSocketUpgrade {
    type Error = WebSocketUpgradeError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let result = upgrade(request);

        if let Err(ref error) = result {
            error!("WebSocket upgrade failed: {error}");
        }

        result
    }
}

/// [`Responder`] returned from [`WebSocketUpgrade::on_upgrade`].
pub struct WebSocketUpgradeResponder {
    upgrade: WebSocketUpgrade,
    callback: Option<DynCallback>,
}

impl std::fmt::Debug for WebSocketUpgradeResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketUpgradeResponder")
            .field("response_protocol", &self.upgrade.response_protocol)
            .field("has_callback", &self.callback.is_some())
            .finish()
    }
}

impl Responder for WebSocketUpgradeResponder {
    type Error = WebSocketUpgradeError;
    fn respond_to(
        mut self,
        _request: &Request,
        response: &mut Response,
    ) -> Result<(), Self::Error> {
        let accept = compute_accept_header(&self.upgrade.key);
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;

        {
            let headers = response.headers_mut();
            headers.insert(
                header::CONNECTION,
                header::HeaderValue::from_static("upgrade"),
            );
            headers.insert(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            );
            headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);

            if let Some(protocol) = &self.upgrade.response_protocol {
                if let Ok(value) = header::HeaderValue::from_str(protocol) {
                    headers.insert(header::SEC_WEBSOCKET_PROTOCOL, value);
                }
            }
        }

        if let Some(callback) = self.callback.take() {
            let on_upgrade = self.upgrade.on_upgrade.clone();
            let config = self.upgrade.config.clone();
            let spawner = self
                .upgrade
                .spawner
                .take()
                .expect("Spawner must be set by the HTTP backend");

            spawner.spawn(async move {
                match on_upgrade.await {
                    Ok(upgraded) => {
                        let io = UpgradedIo(upgraded);
                        let stream =
                            WebSocket::from_raw_socket(TokioAdapter::new(io), Role::Server, config)
                                .await;
                        callback(stream).await;
                    }
                    Err(error) => {
                        error!("WebSocket upgrade failed: {error}");
                    }
                }
            });
        }

        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<crate::openapi::ResponseSchema>> {
        Some(vec![crate::openapi::ResponseSchema {
            status: Some(StatusCode::SWITCHING_PROTOCOLS),
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}

impl From<TungsteniteError> for WebSocketError {
    fn from(error: TungsteniteError) -> Self {
        match error {
            TungsteniteError::Io(err) => Self::Transport(err),
            other => Self::Protocol(other.to_string()),
        }
    }
}

fn to_tungstenite_msg(message: WebSocketMessage) -> TungsteniteMessage {
    match message {
        WebSocketMessage::Text(text) => TungsteniteMessage::Text({
            unsafe { Utf8Bytes::from_bytes_unchecked(text.into_bytes()) }
        }),
        WebSocketMessage::Binary(bytes) => TungsteniteMessage::Binary(bytes),
        WebSocketMessage::Ping(bytes) => TungsteniteMessage::Ping(bytes),
        WebSocketMessage::Pong(bytes) => TungsteniteMessage::Pong(bytes),
        WebSocketMessage::Close => TungsteniteMessage::Close(None),
    }
}

fn to_websocket_msg(message: TungsteniteMessage) -> WebSocketMessage {
    match message {
        TungsteniteMessage::Text(text) => {
            WebSocketMessage::Text(unsafe { ByteStr::from_utf8_unchecked(Bytes::from(text)) })
        }
        TungsteniteMessage::Binary(bytes) => WebSocketMessage::Binary(bytes),
        TungsteniteMessage::Ping(bytes) => WebSocketMessage::Ping(bytes),
        TungsteniteMessage::Pong(bytes) => WebSocketMessage::Pong(bytes),
        TungsteniteMessage::Close(_) => WebSocketMessage::Close,
        TungsteniteMessage::Frame(_) => unimplemented!(),
    }
}

impl From<WebSocketCloseFrame> for TungsteniteCloseFrame {
    fn from(frame: WebSocketCloseFrame) -> Self {
        Self {
            code: CloseCode::from(frame.code),
            reason: Utf8Bytes::from(frame.reason),
        }
    }
}

impl From<TungsteniteCloseFrame> for WebSocketCloseFrame {
    fn from(frame: TungsteniteCloseFrame) -> Self {
        Self {
            code: u16::from(frame.code),
            reason: frame.reason.to_string(),
        }
    }
}

fn to_tungstenite_config(config: &WebSocketConfig) -> TungsteniteConfig {
    let mut cfg = TungsteniteConfig::default();
    cfg.max_message_size = config.max_message_size;
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Body;

    fn create_spawner() -> Spawner {
        // For tests running on tokio, we create a spawner that uses tokio::spawn
        Spawner::from_fn(|fut| {
            tokio::spawn(fut);
        })
    }

    fn build_request() -> Request {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::GET;
        request
    }

    async fn build_valid_upgrade() -> (WebSocketUpgrade, Request) {
        let mut request = build_request();
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_KEY,
            hyper::header::HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        request.headers_mut().insert(
            header::CONNECTION,
            hyper::header::HeaderValue::from_static("Upgrade"),
        );
        request.headers_mut().insert(
            header::UPGRADE,
            hyper::header::HeaderValue::from_static("websocket"),
        );
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_VERSION,
            hyper::header::HeaderValue::from_static("13"),
        );
        let on_upgrade = hyper::upgrade::on(&mut request);
        request.extensions_mut().insert(on_upgrade);
        // Insert spawner like an HTTP backend would
        request.extensions_mut().insert(create_spawner());
        let upgrade = WebSocketUpgrade::extract(&mut request).await.unwrap();
        (upgrade, request)
    }

    #[tokio::test]
    async fn rejects_invalid_headers() {
        let mut request = build_request();
        assert!(WebSocketUpgrade::extract(&mut request).await.is_err());

        request.headers_mut().insert(
            header::SEC_WEBSOCKET_KEY,
            hyper::header::HeaderValue::from_static("x"),
        );
        request.headers_mut().insert(
            header::CONNECTION,
            hyper::header::HeaderValue::from_static("close"),
        );
        request.headers_mut().insert(
            header::UPGRADE,
            hyper::header::HeaderValue::from_static("websocket"),
        );
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_VERSION,
            hyper::header::HeaderValue::from_static("12"),
        );

        assert!(WebSocketUpgrade::extract(&mut request).await.is_err());
    }

    #[tokio::test]
    async fn accepts_valid_request() {
        let (ws, _) = build_valid_upgrade().await;
        assert!(ws.response_protocol.is_none());
    }

    #[tokio::test]
    async fn build_switching_protocols_response() {
        let (upgrade, request) = build_valid_upgrade().await;

        let responder = upgrade.on_upgrade(|_socket| async move {});
        let mut response = Response::new(Body::empty());
        responder
            .respond_to(&request, &mut response)
            .expect("response should build");

        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        let headers = response.headers();
        assert_eq!(
            headers.get(header::UPGRADE),
            Some(&header::HeaderValue::from_static("websocket"))
        );
        assert_eq!(
            headers.get(header::CONNECTION),
            Some(&header::HeaderValue::from_static("upgrade"))
        );
        assert_eq!(
            headers.get(header::SEC_WEBSOCKET_ACCEPT),
            Some(&header::HeaderValue::from_static(
                "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
            ))
        );
    }

    #[tokio::test]
    async fn allows_overriding_max_message_size() {
        let (upgrade, _) = build_valid_upgrade().await;
        let upgrade = upgrade.max_message_size(None);
        assert!(upgrade.config.max_message_size.is_none());
        let upgraded_again = upgrade.max_message_size(Some(512));
        assert_eq!(upgraded_again.config.max_message_size, Some(512));
    }

    // NOTE: Direct WebSocket tests have been moved to hyper/tests/websocket.rs
    // where they can properly test through the full hyper upgrade flow.
}
