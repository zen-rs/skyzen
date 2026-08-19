//! Cloudflare Durable Object websocket adapters.

use skyzen::durable::{
    DurableConnectionsInner, DurableObjectError, WebSocketConnection, WebSocketConnectionInner,
};
use wasm_bindgen::{JsCast, JsValue};
use worker_sys::ext::WebSocketExt;

/// A `Send`/`Sync` wrapper around a Durable Object state handle, shared by
/// the websocket and state adapters.
pub(crate) struct SendSyncDurableState(pub(crate) worker_sys::DurableObjectState);

// SAFETY: Workers WASM executes on a single thread; JS handles are safe to mark Send/Sync.
unsafe impl Send for SendSyncDurableState {}
unsafe impl Sync for SendSyncDurableState {}

impl Clone for SendSyncDurableState {
    fn clone(&self) -> Self {
        Self(clone_state(&self.0))
    }
}

#[derive(Clone)]
struct SendSyncWebSocket(web_sys::WebSocket);

// SAFETY: Workers WASM executes on a single thread; JS handles are safe to mark Send/Sync.
unsafe impl Send for SendSyncWebSocket {}
unsafe impl Sync for SendSyncWebSocket {}

/// Clone a Durable Object state handle through its underlying JS object.
#[must_use]
pub(crate) fn clone_state(
    state: &worker_sys::DurableObjectState,
) -> worker_sys::DurableObjectState {
    let js: &JsValue = state.as_ref();
    js.clone().unchecked_into()
}

/// Cloudflare websocket connection implementation.
pub struct CfWebSocketConnection {
    state: SendSyncDurableState,
    socket: SendSyncWebSocket,
}

impl CfWebSocketConnection {
    /// Create a Cloudflare websocket connection adapter.
    #[must_use]
    pub const fn new(socket: web_sys::WebSocket, state: worker_sys::DurableObjectState) -> Self {
        Self {
            state: SendSyncDurableState(state),
            socket: SendSyncWebSocket(socket),
        }
    }
}

unsafe impl Send for CfWebSocketConnection {}
unsafe impl Sync for CfWebSocketConnection {}

impl std::fmt::Debug for CfWebSocketConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfWebSocketConnection")
            .finish_non_exhaustive()
    }
}

impl WebSocketConnectionInner for CfWebSocketConnection {
    fn send_text(&self, text: &str) -> Result<(), DurableObjectError> {
        self.socket.0.send_with_str(text).map_err(websocket_err)
    }

    fn send_binary(&self, data: &[u8]) -> Result<(), DurableObjectError> {
        self.socket
            .0
            .send_with_u8_array(data)
            .map_err(websocket_err)
    }

    fn close(&self, code: u16, reason: &str) -> Result<(), DurableObjectError> {
        self.socket
            .0
            .close_with_code_and_reason(code, reason)
            .map_err(websocket_err)
    }

    fn tags(&self) -> Result<Vec<String>, DurableObjectError> {
        self.state.0.get_tags(&self.socket.0).map_err(websocket_err)
    }

    fn get_attachment_raw(&self) -> Result<Option<Vec<u8>>, DurableObjectError> {
        let value = self
            .socket
            .0
            .deserialize_attachment()
            .map_err(websocket_err)?;

        if value.is_null() || value.is_undefined() {
            return Ok(None);
        }
        if value.is_instance_of::<js_sys::Uint8Array>() {
            return Ok(Some(js_sys::Uint8Array::new(&value).to_vec()));
        }
        if value.is_instance_of::<js_sys::ArrayBuffer>() {
            return Ok(Some(js_sys::Uint8Array::new(&value).to_vec()));
        }

        Err(DurableObjectError::WebSocket(format!(
            "deserializeAttachment returned unsupported value: {value:?}"
        )))
    }

    fn set_attachment_raw(&self, data: &[u8]) -> Result<(), DurableObjectError> {
        let value: JsValue = js_sys::Uint8Array::from(data).into();
        self.socket
            .0
            .serialize_attachment(value)
            .map_err(websocket_err)
    }
}

/// Cloudflare websocket-connection collection implementation.
#[derive(Clone)]
pub struct CfDurableConnections {
    state: SendSyncDurableState,
}

impl CfDurableConnections {
    /// Create from Durable Object state.
    #[must_use]
    pub const fn new(state: worker_sys::DurableObjectState) -> Self {
        Self {
            state: SendSyncDurableState(state),
        }
    }
}

unsafe impl Send for CfDurableConnections {}
unsafe impl Sync for CfDurableConnections {}

impl std::fmt::Debug for CfDurableConnections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfDurableConnections")
            .finish_non_exhaustive()
    }
}

impl DurableConnectionsInner for CfDurableConnections {
    fn all(&self) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        let sockets = self.state.0.get_websockets().map_err(websocket_err)?;
        let mut connections = Vec::with_capacity(sockets.len());
        for socket in sockets {
            let state = clone_state(&self.state.0);
            connections.push(WebSocketConnection::new(Box::new(
                CfWebSocketConnection::new(socket, state),
            )));
        }
        Ok(connections)
    }

    fn by_tag(&self, tag: &str) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        let sockets = self
            .state
            .0
            .get_websockets_with_tag(tag)
            .map_err(websocket_err)?;
        let mut connections = Vec::with_capacity(sockets.len());
        for socket in sockets {
            let state = clone_state(&self.state.0);
            connections.push(WebSocketConnection::new(Box::new(
                CfWebSocketConnection::new(socket, state),
            )));
        }
        Ok(connections)
    }

    fn set_auto_response(&self, request: &str, response: &str) -> Result<(), DurableObjectError> {
        let pair = worker_sys::WebSocketRequestResponsePair::new(request, response)
            .map_err(websocket_err)?;
        self.state
            .0
            .set_websocket_auto_response(&pair)
            .map_err(websocket_err)
    }

    fn clear_auto_response(&self) -> Result<(), DurableObjectError> {
        let setter = js_sys::Reflect::get(
            self.state.0.as_ref(),
            &JsValue::from_str("setWebSocketAutoResponse"),
        )
        .map_err(websocket_err)?;
        let setter: js_sys::Function = setter.dyn_into().map_err(|value| {
            DurableObjectError::WebSocket(format!(
                "setWebSocketAutoResponse is not callable: {value:?}"
            ))
        })?;
        setter
            .call1(self.state.0.as_ref(), &JsValue::NULL)
            .map_err(websocket_err)?;
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn DurableConnectionsInner> {
        Box::new(self.clone())
    }
}

#[allow(clippy::needless_pass_by_value)]
fn websocket_err(error: JsValue) -> DurableObjectError {
    DurableObjectError::WebSocket(format!("{error:?}"))
}
