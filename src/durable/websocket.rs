//! WebSocket types for Durable Object hibernation.

#[cfg(all(feature = "ws", target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(all(feature = "ws", target_arch = "wasm32"))]
use http_kit::error::BoxHttpError;
#[cfg(all(feature = "ws", target_arch = "wasm32"))]
use http_kit::http_error;
use http_kit::ws::WebSocketMessage;
use serde::{de::DeserializeOwned, Serialize};
#[cfg(all(feature = "ws", target_arch = "wasm32"))]
use skyzen_core::Responder;
#[cfg(all(feature = "ws", target_arch = "wasm32"))]
use wasm_bindgen::JsCast;

use super::error::DurableObjectError;

/// A hibernation WebSocket event delivered to [`DurableObject::websocket`].
///
/// [`DurableObject::websocket`]: super::DurableObject::websocket
#[derive(Debug)]
pub enum WebSocketEvent {
    /// A message was received from the client.
    Message(WebSocketMessage),
    /// The connection was closed by the client.
    Close {
        /// The close code.
        code: u16,
        /// The close reason.
        reason: String,
        /// Whether the close was clean.
        was_clean: bool,
    },
    /// An error occurred on the connection.
    Error(String),
}

/// Opaque handle to a hibernating WebSocket connection.
///
/// This wraps the platform-specific WebSocket object and provides
/// methods to send messages, manage tags, and handle attachments.
///
/// The inner representation is platform-specific and set by the runtime glue.
pub struct WebSocketConnection {
    /// Platform-specific inner handle, set by the cloudflare glue layer.
    inner: Box<dyn WebSocketConnectionInner>,
}

impl std::fmt::Debug for WebSocketConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketConnection")
            .finish_non_exhaustive()
    }
}

/// Platform-specific WebSocket connection operations.
///
/// Implemented by the cloudflare glue layer (or test mocks).
pub trait WebSocketConnectionInner: Send + Sync {
    /// Send a text message.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when the runtime send fails.
    fn send_text(&self, text: &str) -> Result<(), DurableObjectError>;
    /// Send binary data.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when the runtime send fails.
    fn send_binary(&self, data: &[u8]) -> Result<(), DurableObjectError>;
    /// Close the connection.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when the runtime close fails.
    fn close(&self, code: u16, reason: &str) -> Result<(), DurableObjectError>;
    /// Get tags for this connection.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when tag lookup fails.
    fn tags(&self) -> Result<Vec<String>, DurableObjectError>;
    /// Get the typed attachment.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when attachment retrieval fails.
    fn get_attachment_raw(&self) -> Result<Option<Vec<u8>>, DurableObjectError>;
    /// Set the typed attachment.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when attachment persistence fails.
    fn set_attachment_raw(&self, data: &[u8]) -> Result<(), DurableObjectError>;
}

impl WebSocketConnection {
    /// Create from a platform-specific inner handle.
    #[must_use]
    pub fn new(inner: Box<dyn WebSocketConnectionInner>) -> Self {
        Self { inner }
    }

    /// Send a text message to this connection.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the send fails.
    pub fn send_text(&self, text: &str) -> Result<(), DurableObjectError> {
        self.inner.send_text(text)
    }

    /// Send binary data to this connection.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the send fails.
    pub fn send_binary(&self, data: &[u8]) -> Result<(), DurableObjectError> {
        self.inner.send_binary(data)
    }

    /// Serialize a value to JSON and send it as text.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if serialization or send fails.
    pub fn send_json<T: Serialize>(&self, value: &T) -> Result<(), DurableObjectError> {
        let json = serde_json::to_string(value)
            .map_err(|e| DurableObjectError::Serialization(e.to_string()))?;
        self.send_text(&json)
    }

    /// Close this connection with a code and reason.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the close fails.
    pub fn close(&self, code: u16, reason: &str) -> Result<(), DurableObjectError> {
        self.inner.close(code, reason)
    }

    /// Get the tags associated with this connection.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the operation fails.
    pub fn tags(&self) -> Result<Vec<String>, DurableObjectError> {
        self.inner.tags()
    }

    /// Get the per-connection typed attachment (survives hibernation).
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if deserialization fails.
    pub fn attachment<T: DeserializeOwned>(&self) -> Result<Option<T>, DurableObjectError> {
        self.inner
            .get_attachment_raw()?
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|e| DurableObjectError::Serialization(e.to_string()))
            })
            .transpose()
    }

    /// Set the per-connection typed attachment (survives hibernation).
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if serialization fails.
    pub fn set_attachment<T: Serialize>(&self, value: &T) -> Result<(), DurableObjectError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|e| DurableObjectError::Serialization(e.to_string()))?;
        self.inner.set_attachment_raw(&bytes)
    }
}

/// Responder that accepts a WebSocket for Hibernation.
///
/// Construct this in a handler, add tags, and return it.
/// During `respond_to()`, the runtime reads `DurableObjectState` from
/// request extensions to call `acceptWebSocket(ws, tags)`.
#[derive(Debug, Default)]
pub struct HibernationWebSocketUpgrade {
    tags: Vec<String>,
}

impl HibernationWebSocketUpgrade {
    /// Create a new hibernation WebSocket upgrade.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tag to categorize this connection.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Get the tags that will be applied to this connection.
    #[must_use]
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Consume and return the tags.
    #[must_use]
    pub fn into_tags(self) -> Vec<String> {
        self.tags
    }
}

#[cfg(all(feature = "ws", target_arch = "wasm32"))]
type DurableObjectWebSocketAcceptFn =
    dyn Fn(&web_sys::WebSocket, &[String]) -> Result<(), DurableObjectError> + Send + Sync;

/// Wrapper for client websocket stored in response extensions.
#[cfg(all(feature = "ws", target_arch = "wasm32"))]
#[derive(Clone)]
pub struct DurableClientWebSocket(pub web_sys::WebSocket);

#[cfg(all(feature = "ws", target_arch = "wasm32"))]
impl std::fmt::Debug for DurableClientWebSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableClientWebSocket")
            .finish_non_exhaustive()
    }
}

// SAFETY: WebAssembly workers are single-threaded; JS handles are safe to mark Send/Sync.
#[cfg(all(feature = "ws", target_arch = "wasm32"))]
unsafe impl Send for DurableClientWebSocket {}
#[cfg(all(feature = "ws", target_arch = "wasm32"))]
unsafe impl Sync for DurableClientWebSocket {}

/// Internal extension value injected by Durable Object runtime glue.
///
/// `HibernationWebSocketUpgrade` reads this from request extensions and
/// calls `acceptWebSocket(server, tags)` through it.
#[cfg(all(feature = "ws", target_arch = "wasm32"))]
#[derive(Clone)]
pub struct DurableObjectState {
    accept_websocket: Arc<DurableObjectWebSocketAcceptFn>,
}

#[cfg(all(feature = "ws", target_arch = "wasm32"))]
impl std::fmt::Debug for DurableObjectState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableObjectState").finish_non_exhaustive()
    }
}

#[cfg(all(feature = "ws", target_arch = "wasm32"))]
impl DurableObjectState {
    /// Create a state adapter used by `HibernationWebSocketUpgrade`.
    #[must_use]
    pub fn new(
        accept_websocket: impl Fn(&web_sys::WebSocket, &[String]) -> Result<(), DurableObjectError>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            accept_websocket: Arc::new(accept_websocket),
        }
    }

    /// Accept a server websocket with optional tags.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when runtime websocket acceptance fails.
    pub fn accept_websocket(
        &self,
        websocket: &web_sys::WebSocket,
        tags: &[String],
    ) -> Result<(), DurableObjectError> {
        (self.accept_websocket)(websocket, tags)
    }
}

#[cfg(all(feature = "ws", target_arch = "wasm32"))]
http_error!(
    HibernationDurableStateMissing,
    http_kit::StatusCode::INTERNAL_SERVER_ERROR,
    "DurableObjectState missing in request extensions for hibernation websocket upgrade."
);

#[cfg(all(feature = "ws", target_arch = "wasm32"))]
http_error!(
    HibernationWebSocketAcceptFailed,
    http_kit::StatusCode::INTERNAL_SERVER_ERROR,
    "Failed to accept hibernation websocket connection."
);

#[cfg(all(feature = "ws", target_arch = "wasm32"))]
impl Responder for HibernationWebSocketUpgrade {
    type Error = BoxHttpError;

    fn respond_to(
        self,
        request: &crate::Request,
        response: &mut crate::Response,
    ) -> Result<(), Self::Error> {
        let durable_state = request
            .extensions()
            .get::<DurableObjectState>()
            .cloned()
            .ok_or_else(|| Box::new(HibernationDurableStateMissing::new()) as BoxHttpError)?;

        let pair = crate::websocket::ffi::WebSocketPair::new();
        let client = pair.client();
        let server = pair.server();
        let server_socket: web_sys::WebSocket = server.unchecked_into();

        durable_state
            .accept_websocket(&server_socket, self.tags())
            .map_err(|error| {
                tracing::error!(%error, "failed to accept hibernation websocket");
                Box::new(HibernationWebSocketAcceptFailed::new()) as BoxHttpError
            })?;

        *response.status_mut() = http_kit::StatusCode::SWITCHING_PROTOCOLS;
        let client_socket: web_sys::WebSocket = client.unchecked_into();
        response
            .extensions_mut()
            .insert(DurableClientWebSocket(client_socket));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde::{Deserialize, Serialize};

    use super::{
        DurableObjectError, HibernationWebSocketUpgrade, WebSocketConnection,
        WebSocketConnectionInner,
    };

    #[derive(Debug, Default)]
    struct MockSocketState {
        text_messages: Vec<String>,
        binary_messages: Vec<Vec<u8>>,
        close_calls: Vec<(u16, String)>,
        tags: Vec<String>,
        attachment: Option<Vec<u8>>,
        fail_text: Option<DurableObjectError>,
        fail_binary: Option<DurableObjectError>,
        fail_close: Option<DurableObjectError>,
        fail_tags: Option<DurableObjectError>,
        fail_get_attachment: Option<DurableObjectError>,
        fail_set_attachment: Option<DurableObjectError>,
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

        fn close(&self, code: u16, reason: &str) -> Result<(), DurableObjectError> {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_close {
                return Err(Self::error_clone(error));
            }
            state.close_calls.push((code, reason.to_owned()));
            Ok(())
        }

        fn tags(&self) -> Result<Vec<String>, DurableObjectError> {
            let state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_tags {
                return Err(Self::error_clone(error));
            }
            Ok(state.tags.clone())
        }

        fn get_attachment_raw(&self) -> Result<Option<Vec<u8>>, DurableObjectError> {
            let state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_get_attachment {
                return Err(Self::error_clone(error));
            }
            Ok(state.attachment.clone())
        }

        fn set_attachment_raw(&self, data: &[u8]) -> Result<(), DurableObjectError> {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.fail_set_attachment {
                return Err(Self::error_clone(error));
            }
            state.attachment = Some(data.to_vec());
            Ok(())
        }
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Attachment {
        room: String,
        revision: u32,
    }

    #[derive(Debug)]
    struct AlwaysFails;

    impl Serialize for AlwaysFails {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("expected serialization failure"))
        }
    }

    fn connection_with_state(state: Arc<Mutex<MockSocketState>>) -> WebSocketConnection {
        WebSocketConnection::new(Box::new(MockSocketInner { state }))
    }

    #[test]
    fn send_json_serializes_payload_as_text_message() {
        let state = Arc::new(Mutex::new(MockSocketState::default()));
        let connection = connection_with_state(Arc::clone(&state));

        connection
            .send_json(&Attachment {
                room: "room-1".to_owned(),
                revision: 7,
            })
            .unwrap();

        assert_eq!(
            state.lock().unwrap().text_messages,
            vec![r#"{"room":"room-1","revision":7}"#.to_owned()]
        );
    }

    #[test]
    fn send_json_rejects_non_finite_numbers() {
        let connection = connection_with_state(Arc::new(Mutex::new(MockSocketState::default())));

        let error = connection.send_json(&AlwaysFails).unwrap_err();

        assert!(matches!(error, DurableObjectError::Serialization(_)));
    }

    #[test]
    fn attachment_round_trips_through_raw_storage() {
        let state = Arc::new(Mutex::new(MockSocketState::default()));
        let connection = connection_with_state(Arc::clone(&state));
        let attachment = Attachment {
            room: "general".to_owned(),
            revision: 3,
        };

        connection.set_attachment(&attachment).unwrap();
        let restored = connection.attachment::<Attachment>().unwrap().unwrap();

        assert_eq!(restored, attachment);
        assert!(state.lock().unwrap().attachment.is_some());
    }

    #[test]
    fn attachment_reports_deserialization_errors_for_invalid_bytes() {
        let state = Arc::new(Mutex::new(MockSocketState {
            attachment: Some(b"{".to_vec()),
            ..MockSocketState::default()
        }));
        let connection = connection_with_state(state);

        let error = connection.attachment::<Attachment>().unwrap_err();

        assert!(matches!(error, DurableObjectError::Serialization(_)));
    }

    #[test]
    fn close_and_tags_delegate_to_inner_handle() {
        let state = Arc::new(Mutex::new(MockSocketState {
            tags: vec!["room-1".to_owned(), "admin".to_owned()],
            ..MockSocketState::default()
        }));
        let connection = connection_with_state(Arc::clone(&state));

        assert_eq!(
            connection.tags().unwrap(),
            vec!["room-1".to_owned(), "admin".to_owned()]
        );
        connection.close(1000, "done").unwrap();

        assert_eq!(
            state.lock().unwrap().close_calls,
            vec![(1000, "done".to_owned())]
        );
    }

    #[test]
    fn hibernation_upgrade_preserves_tag_order() {
        let tags = HibernationWebSocketUpgrade::new()
            .tag("room-1")
            .tag("presence")
            .into_tags();

        assert_eq!(tags, vec!["room-1".to_owned(), "presence".to_owned()]);
    }
}
