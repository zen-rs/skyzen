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
//!         while let Some(message) = socket.next().await {
//!             if let Some(reply) = message?.into_text() {
//!                 socket.send_text(reply).await?;
//!             }
//!         }
//!         Ok::<_, skyzen::Error>(())
//!     })
//! }
//! ```

use crate::{
    header,
    websocket::{
        session::{internal_error_frame, IntoWebSocketOutcome},
        types::{
            offered_protocols, select_protocol, WebSocketCloseFrame, WebSocketError,
            WebSocketResult,
        },
    },
    Method, Request, Response, StatusCode,
};
use async_tungstenite::{
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
use core::future::{ready, Future};
use executor_core::{AnyExecutor, Executor};
use futures_channel::oneshot;
use futures_core::Stream;
use futures_util::Sink;
use http_kit::utils::{AsyncRead, AsyncWrite};
use http_kit::{
    utils::{ByteStr, Bytes},
    ws::{WebSocketConfig, WebSocketMessage},
};
use hyper::{
    rt::{Read, ReadBuf, Write},
    upgrade::{OnUpgrade, Upgraded},
};
use serde::Serialize;
use skyzen_core::{
    error::{Error, ErrorChain},
    Extractor, Responder,
};
use std::sync::Arc;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tracing::{debug, error};

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

    /// The async executor was not injected by the HTTP backend, so the upgrade cannot be driven.
    #[error("WebSocket runtime executor is unavailable", status = StatusCode::INTERNAL_SERVER_ERROR)]
    MissingExecutor,
}

fn header_has_token(value: &header::HeaderValue, token: &str) -> bool {
    value.to_str().is_ok_and(|value| {
        value
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case(token))
    })
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

/// Upgraded connection wrapper that implements `futures_io` traits.
#[derive(Debug)]
pub struct UpgradedIo(Upgraded);

impl AsyncRead for UpgradedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        let mut hyper_buf = ReadBuf::uninit(unsafe {
            // SAFETY: We're converting &mut [u8] to &mut [MaybeUninit<u8>]
            // This is safe because MaybeUninit<u8> has the same layout as u8
            std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast(), buf.len())
        });
        let cursor = hyper_buf.unfilled();
        match Pin::new(&mut this.0).poll_read(cx, cursor) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(hyper_buf.filled().len())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for UpgradedIo {
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

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

type NativeIo = UpgradedIo;

/// Stream representing a WebSocket connection handled by `async-tungstenite`.
pub struct WebSocket {
    stream: SessionStream,
    config: WebSocketConfig,
}

/// The live connection, plus where it goes once the session handler is done with it.
///
/// A session handler takes the socket by value, so a handler that fails has already given up the
/// only handle to the connection — `async-tungstenite`'s sender half is not cloneable, and
/// `split` consumes the stream, so no second handle exists to close with. Handing the stream back
/// here when the socket is dropped is what lets the framework still send a `1011` close frame
/// after a failed session instead of letting the connection die silently.
struct SessionStream {
    /// `None` only after [`SessionStream::into_stream`], which consumes the whole socket.
    stream: Option<WebSocketStream<NativeIo>>,
    release: Option<oneshot::Sender<WebSocketStream<NativeIo>>>,
}

impl SessionStream {
    const fn stream_mut(&mut self) -> &mut WebSocketStream<NativeIo> {
        self.stream.as_mut().expect(
            "the websocket stream is taken only by `into_stream`, which consumes the socket",
        )
    }

    /// Take the stream out, giving up the ability to hand it back.
    fn into_stream(mut self) -> WebSocketStream<NativeIo> {
        self.release = None;
        self.stream
            .take()
            .expect("`into_stream` consumes the socket, so it runs at most once")
    }
}

impl Drop for SessionStream {
    fn drop(&mut self) {
        if let (Some(stream), Some(release)) = (self.stream.take(), self.release.take()) {
            // The receiver is gone whenever the session succeeded, which is the common case; the
            // stream is simply dropped here then, closing the connection as before.
            let _ = release.send(stream);
        }
    }
}

impl WebSocket {
    pub(crate) async fn from_raw_socket(
        stream: NativeIo,
        role: Role,
        config: WebSocketConfig,
        release: Option<oneshot::Sender<WebSocketStream<NativeIo>>>,
    ) -> Self {
        let inner =
            WebSocketStream::from_raw_socket(stream, role, Some(to_tungstenite_config(&config)))
                .await;
        Self {
            stream: SessionStream {
                stream: Some(inner),
                release,
            },
            config,
        }
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
        self.stream
            .stream_mut()
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
    ///     tracing::info!(value = data.value, "received");
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
        self.stream
            .stream_mut()
            .close(close_frame.map(Into::into))
            .await
            .map_err(WebSocketError::from)
    }

    /// Split the websocket into independent sender and receiver halves.
    ///
    /// # Note
    ///
    /// Splitting gives up the framework's ability to close the connection for a session handler
    /// that returns an error: the halves own the stream from then on. Such a session is still
    /// logged, but sending the [`INTERNAL_ERROR`](super::INTERNAL_ERROR) close frame is then the
    /// handler's own job.
    pub fn split(self) -> (WebSocketSender, WebSocketReceiver) {
        let config = self.config.clone();
        let (inner_sink, inner_stream) = self.stream.into_stream().split();

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
        match Pin::new(self.stream.stream_mut()).poll_next(cx) {
            Poll::Ready(Some(Ok(message))) => Poll::Ready(Some(to_websocket_msg(message))),
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
        Pin::new(self.stream.stream_mut())
            .poll_ready(cx)
            .map_err(WebSocketError::from)
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        item: WebSocketMessage,
    ) -> std::result::Result<(), Self::Error> {
        Pin::new(self.stream.stream_mut())
            .start_send(to_tungstenite_msg(item))
            .map_err(WebSocketError::from)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(self.stream.stream_mut())
            .poll_flush(cx)
            .map_err(WebSocketError::from)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Pin::new(self.stream.stream_mut())
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
            Poll::Ready(Some(Ok(message))) => Poll::Ready(Some(to_websocket_msg(message))),
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
    response_protocol: Option<header::HeaderValue>,
    config: WebSocketConfig,
    executor: Option<Arc<AnyExecutor>>,
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

    /// Set the maximum incoming message size accepted by the websocket.
    ///
    /// Pass `None` to disable the limit enforced by the backend implementation.
    #[must_use]
    pub const fn max_message_size(mut self, max_size: Option<usize>) -> Self {
        self.config.max_message_size = max_size;
        self
    }

    /// Finalize the handshake and start handling the upgraded socket with `callback`.
    ///
    /// The callback owns the connection until it returns. What it returns says how the session
    /// ended: `()` says nothing beyond "it ended", while a `Result<(), E>` — for any `E` that
    /// converts into [`Error`], which includes [`WebSocketError`] — lets the handler use `?` and
    /// report what stopped it. An error is logged with its whole `source()` chain, and the
    /// framework then tries to close the connection with
    /// [`INTERNAL_ERROR`](super::INTERNAL_ERROR).
    ///
    /// # Example
    ///
    /// ```
    /// use futures_util::StreamExt;
    /// use skyzen::{websocket::WebSocketUpgrade, Responder};
    ///
    /// async fn echo(ws: WebSocketUpgrade) -> impl Responder {
    ///     ws.on_upgrade(|mut socket| async move {
    ///         while let Some(message) = socket.next().await {
    ///             if let Some(text) = message?.into_text() {
    ///                 socket.send_text(text).await?;
    ///             }
    ///         }
    ///         Ok::<_, skyzen::Error>(())
    ///     })
    /// }
    /// ```
    pub fn on_upgrade<F, Fut, R>(self, callback: F) -> WebSocketUpgradeResponder
    where
        F: FnOnce(WebSocket) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = R> + Send + 'static,
        R: IntoWebSocketOutcome + 'static,
    {
        WebSocketUpgradeResponder {
            upgrade: self,
            callback: Some(Box::new(move |socket| {
                Box::pin(async move { callback(socket).await.into_outcome() })
                    as WebSocketCallbackFuture
            })),
        }
    }
}

type WebSocketCallbackFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send>>;
type DynCallback = Box<dyn FnOnce(WebSocket) -> WebSocketCallbackFuture + Send + Sync>;

/// Drive one upgraded connection: build the socket, hand it to the session, and report how the
/// session ended.
async fn run_session(on_upgrade: OnUpgrade, config: WebSocketConfig, callback: DynCallback) {
    let upgraded = match on_upgrade.await {
        Ok(upgraded) => upgraded,
        Err(error) => {
            error!("WebSocket upgrade failed: {error}");
            return;
        }
    };

    let (release, released) = oneshot::channel();
    let socket =
        WebSocket::from_raw_socket(UpgradedIo(upgraded), Role::Server, config, Some(release)).await;

    // The session future is dropped at the end of this statement, and dropping the socket with it
    // is what hands the stream back through `released`.
    let outcome = callback(socket).await;

    if let Err(error) = outcome {
        error!(error = %ErrorChain(&error), "websocket session handler failed");
        close_failed_session(released).await;
    }
}

/// Best-effort close for a session that reported an error, so the peer learns the connection died
/// of a server-side failure rather than watching it go quiet.
async fn close_failed_session(mut released: oneshot::Receiver<WebSocketStream<NativeIo>>) {
    match released.try_recv() {
        Ok(Some(mut stream)) => {
            if let Err(error) = stream.close(Some(internal_error_frame().into())).await {
                debug!("failed to close a failed websocket session: {error}");
            }
        }
        // The socket outlived the session future — it was split, or moved into a task of the
        // handler's own — so no handle to close with exists and the error log is all there is.
        Ok(None) | Err(oneshot::Canceled) => {
            debug!("a failed websocket session kept its socket, so no close frame was sent");
        }
    }
}

fn validate_upgrade(
    request: &Request,
) -> Result<(header::HeaderValue, Vec<String>), WebSocketUpgradeError> {
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
            return Err(WebSocketUpgradeError::InvalidConnectionHeader);
        }

        let upgrade_header = headers
            .get(header::UPGRADE)
            .ok_or(WebSocketUpgradeError::MissingUpgradeHeader)?;

        if !upgrade_header
            .to_str()
            .is_ok_and(|value| value.eq_ignore_ascii_case("websocket"))
        {
            return Err(WebSocketUpgradeError::InvalidUpgradeHeader);
        }

        match headers.get(header::SEC_WEBSOCKET_VERSION) {
            Some(version) if version == "13" => {}
            _ => {
                return Err(WebSocketUpgradeError::UnsupportedVersion);
            }
        }

        let requested_protocols = offered_protocols(headers);

        (key, requested_protocols)
    };

    Ok((key, requested_protocols))
}

fn upgrade(request: &mut Request) -> Result<WebSocketUpgrade, WebSocketUpgradeError> {
    let (key, requested_protocols) = validate_upgrade(request)?;
    let on_upgrade = request
        .extensions_mut()
        .remove::<OnUpgrade>()
        .ok_or(WebSocketUpgradeError::MissingOnUpgrade)?;

    // Extract executor from request extensions (injected by the runtime)
    let executor = request.extensions_mut().remove::<Arc<AnyExecutor>>();

    Ok(WebSocketUpgrade {
        key,
        on_upgrade,
        requested_protocols,
        response_protocol: None,
        config: WebSocketConfig::default(),
        executor,
    })
}

/// Build an upgrade from a responder that only receives a shared request reference.
///
/// Hibernating Durable Object upgrades are responders rather than extractors. Hyper's upgrade
/// handle and the executor are cloneable, so they can share the same validated handshake path
/// without duplicating the protocol implementation.
// `websocket::mod` re-exports this module with a glob, so `pub` here would put an internal
// handshake helper in the public API; `pub(crate)` is load-bearing rather than redundant.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn upgrade_from_request(
    request: &Request,
) -> Result<WebSocketUpgrade, WebSocketUpgradeError> {
    let (key, requested_protocols) = validate_upgrade(request)?;
    let on_upgrade = request
        .extensions()
        .get::<OnUpgrade>()
        .cloned()
        .ok_or(WebSocketUpgradeError::MissingOnUpgrade)?;
    let executor = request.extensions().get::<Arc<AnyExecutor>>().cloned();

    Ok(WebSocketUpgrade {
        key,
        on_upgrade,
        requested_protocols,
        response_protocol: None,
        config: WebSocketConfig::default(),
        executor,
    })
}

impl Extractor for WebSocketUpgrade {
    type Error = WebSocketUpgradeError;
    // The handshake is validated from the request's own headers, so the future is ready on
    // creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        let result = upgrade(request);

        if let Err(ref error) = result {
            error!("WebSocket upgrade failed: {error}");
        }

        ready(result)
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

            if let Some(protocol) = self.upgrade.response_protocol.clone() {
                headers.insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
            }
        }

        if let Some(callback) = self.callback.take() {
            let on_upgrade = self.upgrade.on_upgrade.clone();
            let config = self.upgrade.config.clone();
            // Fail closed if the backend never injected an executor, rather than panicking on the
            // request path.
            let executor = self
                .upgrade
                .executor
                .take()
                .ok_or(WebSocketUpgradeError::MissingExecutor)?;

            executor
                .spawn(run_session(on_upgrade, config, callback))
                .detach();
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

fn to_websocket_msg(message: TungsteniteMessage) -> WebSocketResult<WebSocketMessage> {
    match message {
        TungsteniteMessage::Text(text) => Ok(WebSocketMessage::Text(unsafe {
            ByteStr::from_utf8_unchecked(Bytes::from(text))
        })),
        TungsteniteMessage::Binary(bytes) => Ok(WebSocketMessage::Binary(bytes)),
        TungsteniteMessage::Ping(bytes) => Ok(WebSocketMessage::Ping(bytes)),
        TungsteniteMessage::Pong(bytes) => Ok(WebSocketMessage::Pong(bytes)),
        TungsteniteMessage::Close(_) => Ok(WebSocketMessage::Close),
        TungsteniteMessage::Frame(_) => Err(WebSocketError::Protocol(
            "raw websocket frames are not supported".to_string(),
        )),
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
    cfg.max_frame_size = config.max_frame_size;
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Body;
    use executor_core::Task;
    use std::future::Future;

    type Error = Box<dyn std::any::Any + Send>;

    /// Test executor that uses `tokio::spawn` to dispatch tasks
    struct TestTokioExecutor;

    impl executor_core::Executor for TestTokioExecutor {
        type Task<T: Send + 'static> = TestTokioTask<T>;

        fn spawn<Fut>(&self, fut: Fut) -> Self::Task<Fut::Output>
        where
            Fut: std::future::Future<Output: Send> + Send + 'static,
        {
            TestTokioTask(tokio::spawn(fut))
        }
    }

    struct TestTokioTask<T>(tokio::task::JoinHandle<T>);

    impl<T: Send + 'static> Future for TestTokioTask<T> {
        type Output = T;
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            use std::task::Poll;
            match std::pin::Pin::new(&mut self.0).poll(cx) {
                Poll::Ready(Ok(v)) => Poll::Ready(v),
                Poll::Ready(Err(e)) => std::panic::resume_unwind(e.into_panic()),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl<T: Send + 'static> Task<T> for TestTokioTask<T> {
        fn poll_result(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<T, Error>> {
            use std::task::Poll;
            match std::pin::Pin::new(&mut self.0).poll(cx) {
                Poll::Ready(Ok(v)) => Poll::Ready(Ok(v)),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e.into_panic())),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    fn create_executor() -> Arc<AnyExecutor> {
        // For tests running on tokio, we use the current tokio runtime via tokio::spawn
        Arc::new(AnyExecutor::new(TestTokioExecutor))
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
        // Insert executor like an HTTP backend would
        request.extensions_mut().insert(create_executor());
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

    #[test]
    fn frame_messages_surface_protocol_error() {
        use async_tungstenite::tungstenite::protocol::frame::Frame;

        let frame = Frame::ping(Vec::new());
        let err = to_websocket_msg(TungsteniteMessage::Frame(frame))
            .expect_err("frame messages are not supported");
        assert!(matches!(err, WebSocketError::Protocol(_)));
    }

    // NOTE: Direct WebSocket tests live in tests/hyper_websocket.rs
    // where they can properly test through the full hyper upgrade flow.
}
