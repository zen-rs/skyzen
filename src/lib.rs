//! Skyzen — a fast, ergonomic HTTP framework for Rust that works everywhere.
//!
//! Skyzen targets both native servers (Tokio + Hyper) and WebAssembly edge platforms
//! (Cloudflare Workers, Deno Deploy). Write your handlers once, deploy anywhere.
//!
//! # Key Modules
//!
//! - [`routing`] — Tree-based routing with path parameters and HTTP method matching
//! - [`extract`] — Extract typed data from requests (JSON, query strings, path params, headers)
//! - [`responder`] — Convert types into HTTP responses (`Json<T>`, `String`, `StatusCode`, etc.)
//! - [`handler`] — Async functions with extractors as arguments become endpoints automatically
//! - [`utils`] — Common utilities including `Json<T>` and `Redirect`
//! - [`mod@openapi`] — Automatic `OpenAPI` documentation from annotated handlers
//! - [`runtime`] — Runtime primitives for `#[skyzen::main]`
//! - [`websocket`] — Unified WebSocket API across native and WASM (requires `ws` feature)
//!
//! # Getting Started
//!
//! ```rust,ignore
//! use skyzen::routing::{CreateRouteNode, Route, Router};
//!
//! #[skyzen::main]
//! fn main() -> Router {
//!     Route::new((
//!         "/".at(|| async { "Hello, World!" }),
//!     ))
//!     .build()
//! }
//! ```

extern crate self as skyzen;

#[macro_use]
mod macros;

pub mod handler;

pub mod routing;

/// Durable Object abstraction for stateful edge computing.
pub mod durable;

/// OpenAPI helpers.
pub mod openapi;

/// Portable event payloads.
pub mod events;

/// Utilities.
pub mod utils;

/// Runtime primitives leveraged by `#[skyzen::main]`.
pub mod runtime;

/// Attribute & derive macros exported by Skyzen.
pub use skyzen_macros::{
    durable_object, error, import_config, main, openapi, queue, scheduled, test, HttpError,
};

/// Static asset helpers for building file servers.
#[cfg(feature = "static-files")]
pub mod static_files;
#[cfg(feature = "static-files")]
pub use static_files::EmbeddedStaticDir;
#[cfg(all(feature = "static-files", not(target_arch = "wasm32")))]
pub use static_files::StaticDir;

/// Re-exported so the [`embed_dir!`] macro expansion can resolve `include_dir::` types.
#[doc(hidden)]
#[cfg(feature = "static-files")]
pub use include_dir;

#[doc(hidden)]
pub use http_kit;
#[doc(inline)]
pub use http_kit::{
    header, Body, BodyError, Endpoint, HttpError, Method, Middleware, Request, Response,
    StatusCode, Uri,
};
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use js_sys;
#[doc(inline)]
pub use routing::{CreateRouteNode, Route};
pub use skyzen_core::error::*;
pub use skyzen_core::Server;
pub use skyzen_core::{error_response, log_endpoint_error};
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use wasm_bindgen;
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use wasm_bindgen_futures;

/// Hyper-based server backend.
#[cfg(all(feature = "hyper", not(target_arch = "wasm32")))]
pub use skyzen_hyper as hyper;

#[doc(inline)]
pub use openapi::{IgnoreOpenApi, OpenApi, OpenApiOperation};

pub use utoipa::{PartialSchema, ToSchema};

/// Extract strong-typed object from your request.
pub mod extract;

/// Authentication and authorization utilities.
#[cfg(feature = "auth")]
pub mod auth;

pub mod responder;
pub use responder::Responder;

pub mod middleware;

#[doc(hidden)]
pub mod __private {
    use crate::{Endpoint, Middleware};

    pub fn with_middleware<E, M>(
        endpoint: E,
        middleware: M,
    ) -> http_kit::endpoint::WithMiddleware<E, M>
    where
        E: Endpoint,
        M: Middleware,
    {
        http_kit::endpoint::WithMiddleware::new(endpoint, middleware)
    }
}

#[cfg(feature = "ws")]
pub mod websocket;
#[cfg(feature = "ws")]
pub use websocket::{
    WebSocket, WebSocketMessage, WebSocketReceiver, WebSocketSender, WebSocketUpgrade,
};
