//! Skyzen — a fast, ergonomic HTTP framework for Rust that works everywhere.
//!
//! Skyzen targets both native servers (Tokio + Hyper) and WebAssembly edge platforms
//! (Cloudflare Workers, Deno Deploy). Write your handlers once, deploy anywhere.
//!
//! # Key Modules
//!
//! - [`routing`] — Tree-based routing with path parameters, HTTP method matching, and `nest`ing
//!   of an already-built [`Router`](routing::Router)
//! - [`extract`] — Extract typed data from requests: [`Json<T>`](utils::Json),
//!   [`Query<T>`](extract::Query), [`Path<T>`](extract::Path), [`HeaderMap`](header::HeaderMap),
//!   and the body as `Bytes` or `String`
//! - [`responder`] — Convert types into HTTP responses: [`Json<T>`](utils::Json), `String`,
//!   [`StatusCode`], and tuples such as `(StatusCode, Json<T>)`
//! - [`handler`] — Async functions with extractors as arguments become endpoints automatically
//! - [`utils`] — Common utilities including [`Json<T>`](utils::Json),
//!   [`Redirect`](utils::Redirect), [`Html<T>`](utils::Html) and [`CookieJar`](utils::CookieJar)
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
    durable_object, email, embed_migrations, error, import_config, main, openapi, queue, scheduled,
    tail, test, HttpError,
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
    header, Body, BodyError, Endpoint, HttpError, Method, Request, Response, StatusCode, Uri,
};

/// RFC-typed headers, for use with [`TypedHeader`](crate::extract::TypedHeader).
///
/// This is the [`headers`](https://docs.rs/headers) crate; `skyzen::header` beside it is the raw
/// `HeaderName`/`HeaderValue` vocabulary from `http`.
#[cfg(feature = "typed-header")]
pub use headers;
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use js_sys;
#[doc(inline)]
pub use middleware::Middleware;
#[doc(inline)]
pub use routing::{CreateRouteNode, Route};
pub use skyzen_core::error::*;
pub use skyzen_core::Server;
pub use skyzen_core::{error_response, log_endpoint_error, ErrorChain, RequestBodyLimit};
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
    use crate::{
        middleware::{layer, Layered, Middleware},
        Endpoint,
    };

    /// Wrap the endpoint `#[skyzen::main]` produced with one service injector.
    ///
    /// Applied outside `Route::build`, so the services declared in `Skyzen.toml` reach every
    /// request without the route tree having to know about them.
    pub fn with_middleware<E, M>(endpoint: E, middleware: M) -> Layered<E>
    where
        E: Endpoint + Clone + Send + Sync + 'static,
        M: Middleware,
    {
        layer(endpoint, middleware)
    }
}

#[cfg(feature = "ws")]
pub mod websocket;
#[cfg(feature = "ws")]
pub use websocket::{
    WebSocket, WebSocketMessage, WebSocketReceiver, WebSocketSender, WebSocketUpgrade,
};
