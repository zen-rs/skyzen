//! Durable Object abstraction for hibernation-first, stateful edge computing.
//!
//! This module provides the core types for building Durable Objects with Skyzen:
//!
//! - [`DurableObject`] — The trait your DO struct implements
//! - [`DurableContext`] — Service access in non-routed contexts (websocket events)
//! - [`WebSocketEvent`] — Hibernation WebSocket events
//! - [`WebSocketConnection`] — Opaque handle to a hibernating WebSocket
//! - [`HibernationWebSocketUpgrade`] — Responder for accepting hibernating WebSockets
//! - [`DurableConnections`] — Manage all WebSocket connections
//! - [`DurableObjectId`] — The unique identity of a Durable Object instance
//! - [`DurableObjectError`] — Error type for Durable Object operations

mod connections;
mod context;
mod error;
mod id;
mod object;
mod websocket;

pub use connections::DurableConnections;
pub use connections::DurableConnectionsInner;
pub use context::DurableContext;
pub use error::DurableObjectError;
pub use id::DurableObjectId;
pub use object::DurableObject;
#[cfg(all(feature = "ws", target_arch = "wasm32"))]
pub use websocket::DurableClientWebSocket;
#[cfg(all(feature = "ws", target_arch = "wasm32"))]
pub use websocket::DurableObjectState;
pub use websocket::{
    HibernationWebSocketUpgrade, WebSocketConnection, WebSocketConnectionInner, WebSocketEvent,
};
