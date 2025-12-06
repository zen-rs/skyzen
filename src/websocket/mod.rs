//! WebSocket support for both native and WASM targets.
//!
//! **Platform Support:**
//! - ✅ Native (tokio): Full WebSocket support via async-tungstenite
//! - ✅ WASM (WinterCG): WebSocket support via `WebSocketPair` API
//!
//! **Platform Differences:**
//! - WASM: 1 MiB message size limit (platform imposed)
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
//!         while let Some(Ok(msg)) = socket.recv_json::<ChatMessage>().await {
//!             // Echo back with automatic JSON serialization
//!             let _ = socket.send(&msg).await;
//!         }
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
//!         while let Some(Ok(message)) = socket.next().await {
//!             if let Ok(text) = message.into_text() {
//!                 let _ = socket.send_text(text).await;
//!             }
//!         }
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
//!         while let Some(Ok(message)) = socket.next().await {
//!             if let Ok(data) = message.into_binary() {
//!                 let _ = socket.send_binary(data).await;
//!             }
//!         }
//!     })
//! }
//! ```
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
//! # Configuration
//!
//! ```no_run
//! # use skyzen::websocket::WebSocketUpgrade;
//! # use skyzen::Responder;
//! async fn with_config(ws: WebSocketUpgrade) -> impl Responder {
//!     ws.max_message_size(Some(1024 * 1024)) // 1 MB limit
//!         .max_frame_size(Some(64 * 1024))    // 64 KB frame limit
//!         .on_upgrade(|socket| async move {
//!             // Handle connection
//!         })
//! }
//! ```

mod types;

#[cfg(target_arch = "wasm32")]
mod ffi;
#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
pub use types::*;
#[cfg(target_arch = "wasm32")]
pub use wasm::*;
