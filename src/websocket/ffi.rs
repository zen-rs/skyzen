//! FFI bindings for WinterCG WebSocket API.
//!
//! This module provides low-level bindings to the WebSocket API available in
//! WinterCG-compatible runtimes like Cloudflare Workers.

use wasm_bindgen::prelude::*;

/// WebSocketPair represents a pair of WebSocket connections.
///
/// In WinterCG runtimes, this is used for server-side WebSocket upgrades.
#[wasm_bindgen]
extern "C" {
    /// WebSocketPair type from the WinterCG runtime.
    pub type WebSocketPair;

    /// Creates a new WebSocketPair.
    #[wasm_bindgen(constructor)]
    pub fn new() -> WebSocketPair;

    /// Returns the client-side WebSocket (to be returned in Response).
    #[wasm_bindgen(method, getter, js_name = "0")]
    pub fn client(this: &WebSocketPair) -> WebSocket;

    /// Returns the server-side WebSocket (for handling messages).
    #[wasm_bindgen(method, getter, js_name = "1")]
    pub fn server(this: &WebSocketPair) -> WebSocket;
}

/// WebSocket interface for WinterCG runtimes.
#[wasm_bindgen]
extern "C" {
    /// WebSocket type from the WinterCG runtime.
    #[derive(Clone)]
    pub type WebSocket;

    /// Accept the WebSocket connection.
    ///
    /// Must be called on the server-side WebSocket before it can send/receive messages.
    #[wasm_bindgen(method)]
    pub fn accept(this: &WebSocket);

    /// Send data over the WebSocket.
    ///
    /// Accepts either a string or ArrayBuffer/Uint8Array.
    #[wasm_bindgen(method, catch)]
    pub fn send(this: &WebSocket, data: &JsValue) -> Result<(), JsValue>;

    /// Close the WebSocket connection.
    #[wasm_bindgen(method)]
    pub fn close(this: &WebSocket, code: Option<u16>, reason: Option<&str>);

    /// Add an event listener to the WebSocket.
    #[wasm_bindgen(method, js_name = addEventListener)]
    pub fn add_event_listener(this: &WebSocket, event: &str, handler: &js_sys::Function);

    /// Remove an event listener from the WebSocket.
    #[wasm_bindgen(method, js_name = removeEventListener)]
    pub fn remove_event_listener(this: &WebSocket, event: &str, handler: &js_sys::Function);
}

/// MessageEvent received from WebSocket.
#[wasm_bindgen]
extern "C" {
    /// MessageEvent type.
    pub type MessageEvent;

    /// Get the data from the message event.
    #[wasm_bindgen(method, getter)]
    pub fn data(this: &MessageEvent) -> JsValue;
}

/// CloseEvent received when WebSocket closes.
#[wasm_bindgen]
extern "C" {
    /// CloseEvent type.
    pub type CloseEvent;

    /// Get the close code.
    #[wasm_bindgen(method, getter)]
    pub fn code(this: &CloseEvent) -> u16;

    /// Get the close reason.
    #[wasm_bindgen(method, getter)]
    pub fn reason(this: &CloseEvent) -> String;

    /// Whether the connection was closed cleanly.
    #[wasm_bindgen(method, getter, js_name = wasClean)]
    pub fn was_clean(this: &CloseEvent) -> bool;
}

/// ErrorEvent received on WebSocket error.
#[wasm_bindgen]
extern "C" {
    /// ErrorEvent type.
    pub type ErrorEvent;

    /// Get the error message.
    #[wasm_bindgen(method, getter)]
    pub fn message(this: &ErrorEvent) -> String;
}

/// Custom ResponseInit that supports the `webSocket` property for WinterCG runtimes.
///
/// This is required because `web_sys::ResponseInit` doesn't include the `webSocket` field
/// which is a Cloudflare Workers/WinterCG extension to the standard Response API.
#[wasm_bindgen]
extern "C" {
    /// ResponseInit dictionary with WebSocket support.
    #[wasm_bindgen(extends = js_sys::Object)]
    #[derive(Clone, Debug)]
    pub type ResponseInit;
}

impl ResponseInit {
    /// Create a new ResponseInit with WebSocket upgrade configuration.
    pub fn new_websocket(status: u16, websocket: &WebSocket) -> Self {
        let init = js_sys::Object::new();
        js_sys::Reflect::set(&init, &"status".into(), &status.into()).unwrap();
        js_sys::Reflect::set(&init, &"webSocket".into(), websocket).unwrap();
        init.unchecked_into()
    }
}

/// Create a WebSocket upgrade Response for WinterCG runtimes.
///
/// This creates a Response with status 101 and the client WebSocket attached,
/// which is the required format for Cloudflare Workers WebSocket upgrades.
#[wasm_bindgen]
extern "C" {
    /// Create a new Response with options.
    #[wasm_bindgen(js_namespace = Response, js_name = "new", catch)]
    fn response_new_with_init(
        body: JsValue,
        init: &ResponseInit,
    ) -> Result<web_sys::Response, JsValue>;
}

/// Create a WebSocket upgrade response.
pub fn create_websocket_response(client: &WebSocket) -> Result<web_sys::Response, JsValue> {
    let init = ResponseInit::new_websocket(101, client);
    response_new_with_init(JsValue::NULL, &init)
}
