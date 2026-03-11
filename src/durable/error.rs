//! Durable Object error type.

use std::fmt;

/// Errors that can occur during Durable Object operations.
#[derive(Debug)]
pub enum DurableObjectError {
    /// A JavaScript runtime error occurred.
    Runtime(String),

    /// Serialization or deserialization of DO state failed.
    Serialization(String),

    /// A WebSocket operation failed.
    WebSocket(String),
}

impl fmt::Display for DurableObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(msg) => write!(f, "durable object error: {msg}"),
            Self::Serialization(msg) => write!(f, "durable object serialization error: {msg}"),
            Self::WebSocket(msg) => write!(f, "durable object websocket error: {msg}"),
        }
    }
}

impl std::error::Error for DurableObjectError {}
