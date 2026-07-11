//! Shared websocket types exposed by the public API without leaking backend dependencies.
use std::{fmt, io};

/// Result type used by websocket operations.
pub type WebSocketResult<T> = Result<T, WebSocketError>;

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

/// Errors produced by websocket operations.
#[derive(Debug)]
pub enum WebSocketError {
    /// Underlying IO/transport failure.
    Transport(io::Error),
    /// Protocol-level failure.
    Protocol(String),
    /// An outbound message exceeds the configured maximum message size.
    MessageTooLarge {
        /// Size of the rejected message, in bytes.
        len: usize,
        /// Configured maximum message size, in bytes.
        limit: usize,
    },
}

impl From<io::Error> for WebSocketError {
    fn from(error: io::Error) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Display for WebSocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(err) => write!(f, "transport error: {err}"),
            Self::Protocol(err) => write!(f, "protocol error: {err}"),
            Self::MessageTooLarge { len, limit } => write!(
                f,
                "message of {len} bytes exceeds the configured maximum of {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for WebSocketError {}

#[cfg(feature = "json")]
impl From<serde_json::Error> for WebSocketError {
    fn from(error: serde_json::Error) -> Self {
        Self::Protocol(error.to_string())
    }
}
