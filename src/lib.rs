#![warn(missing_docs, missing_debug_implementations)]

//! A simple and fast web server framework.

extern crate self as skyzen;

#[macro_use]
mod macros;

/*#[cfg(test)]
#[macro_use]
mod test_helper;*/

pub mod handler;

pub mod routing;

/// OpenAPI helpers.
pub mod openapi;

/// Utilities.
pub mod utils;

/// Runtime primitives leveraged by `#[skyzen::main]`.
pub mod runtime;

/// Attribute & derive macros exported by Skyzen.
pub use skyzen_macros::{error, main, openapi, HttpError};

/// Static asset helpers for building file servers.
pub mod static_files;
pub use static_files::StaticDir;

#[doc(inline)]
pub use http_kit::{
    header, Body, BodyError, Endpoint, HttpError, Method, Middleware, Request, Response,
    StatusCode, Uri,
};
#[doc(inline)]
pub use routing::{CreateRouteNode, Route};
pub use skyzen_core::error::*;
pub use skyzen_core::Server;

#[doc(inline)]
pub use openapi::{IgnoreOpenApi, OpenApi, OpenApiOperation, OpenApiSchema};

pub use utoipa::{PartialSchema, ToSchema};

/// Extract strong-typed object from your request.
pub mod extract;

pub mod responder;
pub use responder::Responder;

pub mod middleware;

#[cfg(feature = "websocket")]
pub mod websocket;
#[cfg(feature = "websocket")]
pub use websocket::{WebSocket, WebSocketMessage, WebSocketUpgrade};
