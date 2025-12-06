//! Wasm WebSocket stubs.
//!
//! The wasm target re-exports the same API surface so code can compile, but
//! server-side upgrades and raw-socket usage are not supported. All runtime
//! operations return an `Unsupported` error.

use crate::{
    websocket::types::{
        WebSocketCloseFrame, WebSocketConfig, WebSocketError, WebSocketMessage, WebSocketResult,
    },
    Request, Response, StatusCode,
};
use futures_core::Stream;
use futures_util::Sink;
use serde::Serialize;
use skyzen_core::{Extractor, Responder};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

#[skyzen::error(status = StatusCode::NOT_IMPLEMENTED)]
pub enum WebSocketUpgradeError {
    /// WebSocket upgrades are not available on wasm targets.
    #[error("WebSocket upgrades are not supported on wasm targets")]
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct WebSocket {
    config: WebSocketConfig,
}

impl WebSocket {
    pub(crate) async fn from_raw_socket<IO, R>(
        _stream: IO,
        _role: R,
        config: Option<WebSocketConfig>,
    ) -> Self {
        Self {
            config: config.unwrap_or_default(),
        }
    }

    /// Serialize a value to JSON text and send it over the websocket connection.
    pub async fn send<T: Serialize>(&mut self, _value: T) -> WebSocketResult<()> {
        Err(unsupported_error())
    }

    /// Send a raw text frame without JSON serialization.
    pub async fn send_text(&mut self, _text: impl Into<String>) -> WebSocketResult<()> {
        Err(unsupported_error())
    }

    /// Send a [`WebSocketMessage`] without additional processing.
    pub async fn send_message(&mut self, _message: WebSocketMessage) -> WebSocketResult<()> {
        Err(unsupported_error())
    }

    /// Access the underlying websocket configuration.
    pub fn get_config(&self) -> &WebSocketConfig {
        &self.config
    }

    /// Close the websocket connection gracefully.
    pub async fn close(
        &mut self,
        _close_frame: Option<WebSocketCloseFrame>,
    ) -> WebSocketResult<()> {
        Err(unsupported_error())
    }

    /// Split the websocket into independent sender and receiver halves.
    pub fn split(self) -> (WebSocketSender, WebSocketReceiver) {
        let config = self.config.clone();
        (
            WebSocketSender {
                config: config.clone(),
            },
            WebSocketReceiver { config },
        )
    }
}

impl Stream for WebSocket {
    type Item = WebSocketResult<WebSocketMessage>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(Some(Err(unsupported_error())))
    }
}

impl Sink<WebSocketMessage> for WebSocket {
    type Error = WebSocketError;

    fn poll_ready(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Err(unsupported_error()))
    }

    fn start_send(
        self: Pin<&mut Self>,
        _item: WebSocketMessage,
    ) -> std::result::Result<(), Self::Error> {
        Err(unsupported_error())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Err(unsupported_error()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Err(unsupported_error()))
    }
}

/// Sender half returned from [`WebSocket::split`] (unsupported on wasm).
pub struct WebSocketSender {
    config: WebSocketConfig,
}

impl WebSocketSender {
    /// Serialize a value to JSON text and send it over the websocket connection.
    pub async fn send<T: Serialize>(&mut self, _value: T) -> WebSocketResult<()> {
        Err(unsupported_error())
    }

    /// Send a raw text frame without JSON serialization.
    pub async fn send_text(&mut self, _text: impl Into<String>) -> WebSocketResult<()> {
        Err(unsupported_error())
    }

    /// Send a [`WebSocketMessage`] without additional processing.
    pub async fn send_message(&mut self, _message: WebSocketMessage) -> WebSocketResult<()> {
        Err(unsupported_error())
    }

    /// Close the websocket connection gracefully.
    pub async fn close(
        &mut self,
        _close_frame: Option<WebSocketCloseFrame>,
    ) -> WebSocketResult<()> {
        Err(unsupported_error())
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

impl Sink<WebSocketMessage> for WebSocketSender {
    type Error = WebSocketError;

    fn poll_ready(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Err(unsupported_error()))
    }

    fn start_send(
        self: Pin<&mut Self>,
        _item: WebSocketMessage,
    ) -> std::result::Result<(), Self::Error> {
        Err(unsupported_error())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Err(unsupported_error()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Err(unsupported_error()))
    }
}

/// Receiver half returned from [`WebSocket::split`] (unsupported on wasm).
pub struct WebSocketReceiver {
    config: WebSocketConfig,
}

impl WebSocketReceiver {
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

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(Some(Err(unsupported_error())))
    }
}

/// Helper that contains the state required to accept a WebSocket connection.
#[derive(Debug, Clone)]
pub struct WebSocketUpgrade;

impl WebSocketUpgrade {
    /// Negotiate the sub-protocol returned to the client (noop on wasm).
    #[must_use]
    pub fn protocols<I, S>(self, _protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self
    }

    /// Override the [`WebSocketConfig`] used for the upgraded stream (noop on wasm).
    #[must_use]
    pub const fn config(self, _config: WebSocketConfig) -> Self {
        self
    }

    /// Set the maximum incoming message size accepted by the websocket (noop on wasm).
    #[must_use]
    pub const fn max_message_size(self, _max_size: Option<usize>) -> Self {
        self
    }

    /// Finalize the handshake and start handling the upgraded socket with `callback`.
    pub fn on_upgrade<F, Fut>(self, _callback: F) -> WebSocketUpgradeResponder
    where
        F: FnOnce(WebSocket) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        WebSocketUpgradeResponder
    }
}

impl Extractor for WebSocketUpgrade {
    type Error = WebSocketUpgradeError;
    async fn extract(_request: &mut Request) -> Result<Self, Self::Error> {
        Err(WebSocketUpgradeError::Unsupported)
    }
}

/// [`Responder`] returned from [`WebSocketUpgrade::on_upgrade`].
#[derive(Debug, Clone)]
pub struct WebSocketUpgradeResponder;

impl Responder for WebSocketUpgradeResponder {
    type Error = WebSocketUpgradeError;
    fn respond_to(self, _request: &Request, _response: &mut Response) -> Result<(), Self::Error> {
        Err(WebSocketUpgradeError::Unsupported)
    }
}

fn unsupported_error() -> WebSocketError {
    WebSocketError::Protocol("WebSocket upgrades are not supported on wasm targets".to_owned())
}
