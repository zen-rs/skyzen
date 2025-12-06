//! Shared websocket types exposed by the public API without leaking backend dependencies.
use std::{fmt, io};

/// Result type used by websocket operations.
pub type WebSocketResult<T> = Result<T, WebSocketError>;

/// Lightweight websocket configuration used across targets.
#[derive(Clone, Debug, Default)]
pub struct WebSocketConfig {
    /// Maximum incoming message size in bytes. `None` removes the limit.
    pub max_message_size: Option<usize>,
}

/// Close frame representation that avoids depending on tungstenite types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSocketCloseFrame {
    /// Close code sent to peer.
    pub code: u16,
    /// Human readable close reason.
    pub reason: String,
}

impl WebSocketCloseFrame {
    /// Build a close frame from code and reason.
    pub fn new(code: u16, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

/// Message wrapper that mirrors the common websocket frames we need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebSocketMessage {
    /// Text payload.
    Text(String),
    /// Binary payload.
    Binary(Vec<u8>),
    /// Ping control frame.
    Ping(Vec<u8>),
    /// Pong control frame.
    Pong(Vec<u8>),
    /// Close control frame.
    Close(Option<WebSocketCloseFrame>),
}

impl WebSocketMessage {
    /// Create a text message.
    pub fn text(value: impl Into<String>) -> Self {
        WebSocketMessage::Text(value.into())
    }

    /// Create a binary message.
    pub fn binary(bytes: impl Into<Vec<u8>>) -> Self {
        WebSocketMessage::Binary(bytes.into())
    }

    /// Returns true when the message is textual.
    pub fn is_text(&self) -> bool {
        matches!(self, WebSocketMessage::Text(_))
    }

    /// Consume and return the text payload if present.
    pub fn into_text(self) -> Result<String, Self> {
        match self {
            WebSocketMessage::Text(text) => Ok(text),
            other => Err(other),
        }
    }
}

/// Errors produced by websocket operations.
#[derive(Debug)]
pub enum WebSocketError {
    /// Underlying IO/transport failure.
    Transport(io::Error),
    /// Protocol-level failure.
    Protocol(String),
}

impl From<io::Error> for WebSocketError {
    fn from(error: io::Error) -> Self {
        WebSocketError::Transport(error)
    }
}

impl fmt::Display for WebSocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebSocketError::Transport(err) => write!(f, "transport error: {err}"),
            WebSocketError::Protocol(err) => write!(f, "protocol error: {err}"),
        }
    }
}

impl std::error::Error for WebSocketError {}

#[cfg(feature = "json")]
impl From<serde_json::Error> for WebSocketError {
    fn from(error: serde_json::Error) -> Self {
        WebSocketError::Protocol(error.to_string())
    }
}
