//! WebSocket support for both native and WASM targets.
//!
//! **Platform Support:**
//! - ✅ Native (tokio): Full WebSocket support via async-tungstenite
//! - ✅ WASM (WinterCG): WebSocket support via `WebSocketPair` API
//!
//! **Platform Differences:**
//! - WASM: outbound message size is enforced from `WebSocketConfig::max_message_size`; the host
//!   runtime may also impose its own cap (e.g. Cloudflare Workers limits messages to 1 MiB)
//! - WASM: No custom ping/pong frame control
//! - WASM: Event-driven model vs native stream model
//!
//! # Quick Start
//!
//! ## JSON Messages
//!
//! ```no_run
//! use futures_util::StreamExt;
//! use skyzen::websocket::{WebSocketUpgrade, WebSocketMessage};
//! use skyzen::Responder;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Serialize, Deserialize)]
//! struct ChatMessage {
//!     user: String,
//!     text: String,
//! }
//!
//! async fn chat_handler(ws: WebSocketUpgrade) -> impl Responder {
//!     ws.on_upgrade(|mut socket| async move {
//!         // Receive JSON messages using the convenient recv_json method
//!         while let Some(message) = socket.recv_json::<ChatMessage>().await {
//!             // Echo back with automatic JSON serialization
//!             socket.send(&message?).await?;
//!         }
//!         Ok::<_, skyzen::Error>(())
//!     })
//! }
//! ```
//!
//! ## Text Messages
//!
//! ```no_run
//! # use futures_util::StreamExt;
//! # use skyzen::websocket::{WebSocketUpgrade, WebSocketMessage};
//! # use skyzen::Responder;
//! async fn text_echo(ws: WebSocketUpgrade) -> impl Responder {
//!     ws.on_upgrade(|mut socket| async move {
//!         while let Some(message) = socket.next().await {
//!             if let Some(text) = message?.into_text() {
//!                 socket.send_text(text).await?;
//!             }
//!         }
//!         Ok::<_, skyzen::Error>(())
//!     })
//! }
//! ```
//!
//! ## Binary Messages
//!
//! ```no_run
//! # use futures_util::StreamExt;
//! # use skyzen::websocket::{WebSocketUpgrade, WebSocketMessage};
//! # use skyzen::Responder;
//! async fn binary_echo(ws: WebSocketUpgrade) -> impl Responder {
//!     ws.on_upgrade(|mut socket| async move {
//!         while let Some(message) = socket.next().await {
//!             if let Some(data) = message?.into_bytes() {
//!                 socket.send_binary(data).await?;
//!             }
//!         }
//!         Ok::<_, skyzen::Error>(())
//!     })
//! }
//! ```
//!
//! # Reporting a failed session
//!
//! A session handler may return `()` or a `Result<(), E>` for any `E` that converts into
//! [`skyzen::Error`](crate::Error) — [`WebSocketError`] included, so `?` works on every socket
//! operation. An error ends the session, is logged with its whole `source()` chain, and closes the
//! connection with [`INTERNAL_ERROR`] (`1011`) so the peer can tell a server-side failure from a
//! clean goodbye. Returning `()` keeps the older behaviour: whatever the handler swallows stays
//! swallowed.
//!
//! # Convenience Methods
//!
//! The `WebSocket` type provides several convenience methods for common operations:
//!
//! - **JSON**: `send(&value)` for serialization, `recv_json::<T>()` for deserialization
//! - **Text**: `send_text(string)` for plain text messages
//! - **Binary**: `send_binary(bytes)` for binary data
//! - **Ping/Pong**: `send_ping(data)` and `send_pong(data)` (native only)
//!
//! # Protocol Negotiation
//!
//! ```no_run
//! # use skyzen::websocket::WebSocketUpgrade;
//! # use skyzen::Responder;
//! async fn with_protocols(ws: WebSocketUpgrade) -> impl Responder {
//!     ws.protocols(["chat", "superchat"])
//!         .on_upgrade(|socket| async move {
//!             // Handle connection
//!         })
//! }
//! ```
//!
//! The answer is echoed in the `101`'s `Sec-WebSocket-Protocol`, and that echo is mandatory:
//! RFC 6455 §4.1 has a client that offered a subprotocol **fail the connection** when the
//! handshake comes back without one.
//!
//! ## Authenticating a browser socket
//!
//! The browser `WebSocket` constructor sends no custom headers — no `Authorization`, no cookie you
//! control — so the subprotocol list is the only in-band channel a page has for a credential:
//!
//! ```js
//! new WebSocket(url, [`app.bearer.${token}`])
//! ```
//!
//! A fixed list of supported names cannot match a value that carries a token, so read the offer
//! and answer it verbatim. [`RequestedSubprotocols`] does both halves, and works the same in a
//! Durable Object, where the upgrade is constructed rather than extracted:
//!
//! ```no_run
//! # use skyzen::websocket::{RequestedSubprotocols, WebSocketError, WebSocketUpgrade};
//! # use skyzen::Responder;
//! const PREFIX: &str = "app.bearer.";
//!
//! async fn authenticated(
//!     ws: WebSocketUpgrade,
//!     offered: RequestedSubprotocols,
//! ) -> Result<impl Responder, WebSocketError> {
//!     let token = offered
//!         .iter()
//!         .find_map(|protocol| protocol.strip_prefix(PREFIX))
//!         .ok_or_else(|| WebSocketError::Protocol("no bearer subprotocol".to_owned()))?;
//!     // ... verify `token` ...
//!
//!     let answer = offered
//!         .answer(|protocol| protocol.starts_with(PREFIX))
//!         .ok_or_else(|| WebSocketError::Protocol("unanswerable subprotocol".to_owned()))?;
//!     Ok(ws.protocol(answer).on_upgrade(|socket| async move {
//!         // Handle connection
//!     }))
//! }
//! ```
//!
//! # Configuration
//!
//! ```no_run
//! # use skyzen::websocket::{WebSocketConfig, WebSocketUpgrade};
//! # use skyzen::Responder;
//! async fn with_config(ws: WebSocketUpgrade) -> impl Responder {
//!     let config = WebSocketConfig::default()
//!         .with_max_message_size(Some(1024 * 1024)) // 1 MB limit
//!         .with_max_frame_size(Some(64 * 1024)); // 64 KB frame limit
//!
//!     ws.config(config)
//!         .on_upgrade(|socket| async move {
//!             // Handle connection
//!         })
//! }
//! ```

mod session;
mod types;

#[cfg(target_arch = "wasm32")]
pub(crate) mod ffi;
#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod wasm;

pub use http_kit::ws::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::upgrade_from_request;
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
pub(crate) use session::session_handler;
pub use session::{IntoWebSocketOutcome, MaybeSend, MaybeSync, INTERNAL_ERROR};
pub(crate) use types::select_offered_protocol;
pub use types::*;
#[cfg(target_arch = "wasm32")]
pub use wasm::*;
