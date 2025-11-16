#![deny(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

//! A simple and fast web server framework.

#[macro_use]
mod macros;

/*#[cfg(test)]
#[macro_use]
mod test_helper;*/

pub mod handler;

pub mod routing;

/// Utilities.
pub mod utils;

/// Runtime primitives leveraged by `#[skyzen::main]`.
pub mod runtime;

/// Attribute macro for bootstrapping Skyzen applications.
pub use skyzen_macros::main;

/// Static asset helpers for building file servers.
pub mod static_files;
pub use static_files::StaticDir;

#[doc(inline)]
pub use http_kit::{
    header, Body, Endpoint, Error, Method, Middleware, Request, Response, Result, ResultExt,
    StatusCode, Uri,
};
#[doc(inline)]
pub use routing::{CreateRouteNode, Route};
pub use skyzen_core::Server;

/// Extract strong-typed object from your request.
pub mod extract;

pub mod responder;
pub use responder::Responder;

pub mod middleware;

#[cfg(feature = "websocket")]
pub mod websocket;
#[cfg(feature = "websocket")]
pub use websocket::{WebSocket, WebSocketMessage, WebSocketUpgrade};
