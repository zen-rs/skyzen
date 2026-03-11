//! Durable Object WebSocket connection management.

use http_kit::http_error;
use serde::Serialize;
use skyzen_core::{Extractor, StatusCode};

use super::error::DurableObjectError;
use super::websocket::WebSocketConnection;

/// Platform-specific connection management operations.
///
/// Implemented by the Cloudflare glue layer (or test mocks).
pub trait DurableConnectionsInner: Send + Sync {
    /// Get all connected `WebSockets`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when the runtime cannot list connections.
    fn all(&self) -> Result<Vec<WebSocketConnection>, DurableObjectError>;
    /// Get `WebSockets` matching a tag.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when the runtime cannot list tagged connections.
    fn by_tag(&self, tag: &str) -> Result<Vec<WebSocketConnection>, DurableObjectError>;
    /// Set the auto-response pair.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when auto-response configuration fails.
    fn set_auto_response(&self, request: &str, response: &str) -> Result<(), DurableObjectError>;
    /// Clear the auto-response pair.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when clearing auto-response fails.
    fn clear_auto_response(&self) -> Result<(), DurableObjectError>;
    /// Clone into a boxed trait object.
    fn clone_box(&self) -> Box<dyn DurableConnectionsInner>;
}

/// Manage all WebSocket connections on a Durable Object.
///
/// Available as an extractor in `fetch` handlers and via
/// [`DurableContext`](super::DurableContext) in `websocket` handlers.
pub struct DurableConnections {
    inner: Box<dyn DurableConnectionsInner>,
}

impl Clone for DurableConnections {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_box(),
        }
    }
}

impl std::fmt::Debug for DurableConnections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableConnections").finish_non_exhaustive()
    }
}

impl DurableConnections {
    /// Create from a platform-specific inner handle.
    #[must_use]
    pub fn new(inner: Box<dyn DurableConnectionsInner>) -> Self {
        Self { inner }
    }

    /// Get all connected `WebSockets`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the operation fails.
    pub fn all(&self) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        self.inner.all()
    }

    /// Get `WebSockets` matching a tag.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the operation fails.
    pub fn by_tag(&self, tag: &str) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        self.inner.by_tag(tag)
    }

    /// Broadcast a text message to all connections.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if any send fails.
    pub fn broadcast_text(&self, text: &str) -> Result<(), DurableObjectError> {
        for conn in self.all()? {
            conn.send_text(text)?;
        }
        Ok(())
    }

    /// Broadcast binary data to all connections.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if any send fails.
    pub fn broadcast_binary(&self, data: &[u8]) -> Result<(), DurableObjectError> {
        for conn in self.all()? {
            conn.send_binary(data)?;
        }
        Ok(())
    }

    /// Broadcast a JSON-serialized value to all connections.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if serialization or any send fails.
    pub fn broadcast_json<T: Serialize>(&self, value: &T) -> Result<(), DurableObjectError> {
        let json = serde_json::to_string(value)
            .map_err(|e| DurableObjectError::Serialization(e.to_string()))?;
        self.broadcast_text(&json)
    }

    /// Set the auto-response pair for ping/pong-style keep-alive.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the operation fails.
    pub fn set_auto_response(
        &self,
        request: &str,
        response: &str,
    ) -> Result<(), DurableObjectError> {
        self.inner.set_auto_response(request, response)
    }

    /// Clear the auto-response pair.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the operation fails.
    pub fn clear_auto_response(&self) -> Result<(), DurableObjectError> {
        self.inner.clear_auto_response()
    }
}

http_error!(
    /// The `DurableConnections` service was not found in request extensions.
    pub DurableConnectionsNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "DurableConnections not configured. Ensure a DurableConnectionsInner implementation is injected."
);

impl Extractor for DurableConnections {
    type Error = DurableConnectionsNotConfigured;

    async fn extract(request: &mut http_kit::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(DurableConnectionsNotConfigured::new())
    }
}
