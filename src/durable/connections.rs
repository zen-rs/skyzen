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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use http_kit::HttpError;
    use serde::Serialize;
    use skyzen_core::Extractor;

    use super::{DurableConnections, DurableConnectionsInner};
    use crate::durable::{
        error::DurableObjectError,
        websocket::{WebSocketConnection, WebSocketConnectionInner},
    };
    use crate::{Body, Request, StatusCode};

    #[derive(Debug, Default)]
    struct MockSocketState {
        text_messages: Vec<String>,
        binary_messages: Vec<Vec<u8>>,
        tags: Vec<String>,
        fail_text: Option<DurableObjectError>,
        fail_binary: Option<DurableObjectError>,
    }

    #[derive(Debug, Clone)]
    struct MockSocketInner {
        state: Arc<Mutex<MockSocketState>>,
    }

    impl MockSocketInner {
        fn error_clone(error: &DurableObjectError) -> DurableObjectError {
            match error {
                DurableObjectError::Runtime(message) => {
                    DurableObjectError::Runtime(message.clone())
                }
                DurableObjectError::Serialization(message) => {
                    DurableObjectError::Serialization(message.clone())
                }
                DurableObjectError::WebSocket(message) => {
                    DurableObjectError::WebSocket(message.clone())
                }
            }
        }
    }

    impl WebSocketConnectionInner for MockSocketInner {
        fn send_text(&self, text: &str) -> Result<(), DurableObjectError> {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_text {
                return Err(Self::error_clone(error));
            }
            state.text_messages.push(text.to_owned());
            Ok(())
        }

        fn send_binary(&self, data: &[u8]) -> Result<(), DurableObjectError> {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_binary {
                return Err(Self::error_clone(error));
            }
            state.binary_messages.push(data.to_vec());
            Ok(())
        }

        fn close(&self, _code: u16, _reason: &str) -> Result<(), DurableObjectError> {
            Ok(())
        }

        fn tags(&self) -> Result<Vec<String>, DurableObjectError> {
            Ok(self.state.lock().unwrap().tags.clone())
        }

        fn get_attachment_raw(&self) -> Result<Option<Vec<u8>>, DurableObjectError> {
            Ok(None)
        }

        fn set_attachment_raw(&self, _data: &[u8]) -> Result<(), DurableObjectError> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct MockConnectionsState {
        sockets: Vec<Arc<Mutex<MockSocketState>>>,
        auto_response: Option<(String, String)>,
        clear_calls: usize,
        fail_all: Option<DurableObjectError>,
        fail_by_tag: Option<DurableObjectError>,
        fail_set_auto_response: Option<DurableObjectError>,
        fail_clear_auto_response: Option<DurableObjectError>,
    }

    #[derive(Debug, Clone)]
    struct MockConnectionsInner {
        state: Arc<Mutex<MockConnectionsState>>,
    }

    impl MockConnectionsInner {
        fn new(state: Arc<Mutex<MockConnectionsState>>) -> Self {
            Self { state }
        }

        fn error_clone(error: &DurableObjectError) -> DurableObjectError {
            match error {
                DurableObjectError::Runtime(message) => {
                    DurableObjectError::Runtime(message.clone())
                }
                DurableObjectError::Serialization(message) => {
                    DurableObjectError::Serialization(message.clone())
                }
                DurableObjectError::WebSocket(message) => {
                    DurableObjectError::WebSocket(message.clone())
                }
            }
        }

        fn socket_connection(state: Arc<Mutex<MockSocketState>>) -> WebSocketConnection {
            WebSocketConnection::new(Box::new(MockSocketInner { state }))
        }
    }

    impl DurableConnectionsInner for MockConnectionsInner {
        fn all(&self) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
            let state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_all {
                return Err(Self::error_clone(error));
            }
            Ok(state
                .sockets
                .iter()
                .cloned()
                .map(Self::socket_connection)
                .collect())
        }

        fn by_tag(&self, tag: &str) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
            let state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_by_tag {
                return Err(Self::error_clone(error));
            }
            Ok(state
                .sockets
                .iter()
                .filter(|socket| socket.lock().unwrap().tags.iter().any(|value| value == tag))
                .cloned()
                .map(Self::socket_connection)
                .collect())
        }

        fn set_auto_response(
            &self,
            request: &str,
            response: &str,
        ) -> Result<(), DurableObjectError> {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_set_auto_response {
                return Err(Self::error_clone(error));
            }
            state.auto_response = Some((request.to_owned(), response.to_owned()));
            Ok(())
        }

        fn clear_auto_response(&self) -> Result<(), DurableObjectError> {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_clear_auto_response {
                return Err(Self::error_clone(error));
            }
            state.auto_response = None;
            state.clear_calls += 1;
            Ok(())
        }

        fn clone_box(&self) -> Box<dyn DurableConnectionsInner> {
            Box::new(self.clone())
        }
    }

    #[derive(Debug, Serialize)]
    struct BroadcastPayload {
        room: &'static str,
    }

    fn socket(tags: &[&str]) -> Arc<Mutex<MockSocketState>> {
        Arc::new(Mutex::new(MockSocketState {
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            ..MockSocketState::default()
        }))
    }

    fn connections_with_state(state: Arc<Mutex<MockConnectionsState>>) -> DurableConnections {
        DurableConnections::new(Box::new(MockConnectionsInner::new(state)))
    }

    #[test]
    fn broadcast_json_sends_to_all_connections() {
        let first = socket(&["room-1"]);
        let second = socket(&["room-1", "admin"]);
        let state = Arc::new(Mutex::new(MockConnectionsState {
            sockets: vec![Arc::clone(&first), Arc::clone(&second)],
            ..MockConnectionsState::default()
        }));
        let connections = connections_with_state(state);

        connections
            .broadcast_json(&BroadcastPayload { room: "room-1" })
            .unwrap();

        assert_eq!(
            first.lock().unwrap().text_messages,
            vec![r#"{"room":"room-1"}"#.to_owned()]
        );
        assert_eq!(
            second.lock().unwrap().text_messages,
            vec![r#"{"room":"room-1"}"#.to_owned()]
        );
    }

    #[test]
    fn by_tag_returns_only_matching_connections() {
        let room = socket(&["room-1"]);
        let other = socket(&["room-2"]);
        let state = Arc::new(Mutex::new(MockConnectionsState {
            sockets: vec![room, other],
            ..MockConnectionsState::default()
        }));
        let connections = connections_with_state(state);

        let matches = connections.by_tag("room-1").unwrap();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].tags().unwrap(), vec!["room-1".to_owned()]);
    }

    #[test]
    fn broadcast_binary_propagates_socket_failures() {
        let failing = socket(&["room-1"]);
        failing.lock().unwrap().fail_binary =
            Some(DurableObjectError::WebSocket("send failed".to_owned()));
        let state = Arc::new(Mutex::new(MockConnectionsState {
            sockets: vec![failing],
            ..MockConnectionsState::default()
        }));
        let connections = connections_with_state(state);

        let error = connections.broadcast_binary(b"ping").unwrap_err();

        assert!(matches!(error, DurableObjectError::WebSocket(_)));
    }

    #[test]
    fn auto_response_operations_delegate_to_inner_handle() {
        let state = Arc::new(Mutex::new(MockConnectionsState::default()));
        let connections = connections_with_state(Arc::clone(&state));

        connections.set_auto_response("ping", "pong").unwrap();
        connections.clear_auto_response().unwrap();

        let state = state.lock().unwrap();
        assert_eq!(state.auto_response, None);
        assert_eq!(state.clear_calls, 1);
    }

    #[tokio::test]
    async fn extractor_reads_injected_connections_from_request_extensions() {
        let state = Arc::new(Mutex::new(MockConnectionsState::default()));
        let connections = connections_with_state(state);
        let mut request = Request::new(Body::empty());
        request.extensions_mut().insert(connections.clone());

        let extracted = DurableConnections::extract(&mut request).await.unwrap();

        extracted.set_auto_response("ping", "pong").unwrap();
    }

    #[tokio::test]
    async fn extractor_errors_when_connections_are_missing() {
        let mut request = Request::new(Body::empty());

        let error = DurableConnections::extract(&mut request).await.unwrap_err();

        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
