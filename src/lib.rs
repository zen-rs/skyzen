//! Skyzen — an HTTP framework for Rust whose infrastructure is portable, not just its handlers.
//!
//! A handler asks for [`Kv`], [`Storage`], [`Queue`] or [`Db`] and gets a *capability*, not a
//! vendor SDK. The same function body runs against Redis and Postgres on a server, Cloudflare KV
//! and D1 at the edge, `DynamoDB` and SQS on AWS, Cosmos DB and Blob Storage on Azure — and against
//! in-process fakes from `skyzen-test` in a plain `cargo test`, with no sockets and no emulator.
//! Those types live in [`skyzen_services`](https://docs.rs/skyzen-services).
//!
//! The HTTP layer is portable in the same way: one `#[skyzen::main]` serves a native Tokio/Hyper
//! server, an AWS Lambda, an Azure Functions custom handler, and a Cloudflare Worker, chosen by
//! what the process finds in its environment rather than by anything in the application.
//!
//! [`Kv`]: https://docs.rs/skyzen-services/latest/skyzen_services/struct.Kv.html
//! [`Storage`]: https://docs.rs/skyzen-services/latest/skyzen_services/struct.Storage.html
//! [`Queue`]: https://docs.rs/skyzen-services/latest/skyzen_services/struct.Queue.html
//! [`Db`]: https://docs.rs/skyzen-services/latest/skyzen_services/struct.Db.html
//!
//! # Key Modules
//!
//! - [`routing`] — Tree-based routing with path parameters, HTTP method matching, custom 404/405
//!   handlers, and `nest`ing of an already-built [`Router`](routing::Router)
//! - [`extract`] — Extract typed data from requests: [`Json<T>`](utils::Json),
//!   [`Query<T>`](extract::Query), [`Path<T>`](extract::Path),
//!   [`TypedHeader<H>`](extract::TypedHeader), [`HeaderMap`](header::HeaderMap), and the body as
//!   `Bytes` or `String`
//! - [`responder`] — Convert types into HTTP responses: [`Json<T>`](utils::Json), `String`,
//!   [`StatusCode`], [`Sse`](responder::Sse), and tuples such as `(StatusCode, Json<T>)`
//! - [`handler`] — Async functions with extractors as arguments become endpoints automatically
//! - [`middleware`] — `&self` middleware, [`from_fn`](middleware::from_fn), and the shipped
//!   [`Cors`](middleware::Cors), [`BodyLimit`](middleware::BodyLimit) and compression layers
//! - [`utils`] — Common utilities including [`Json<T>`](utils::Json),
//!   [`Redirect`](utils::Redirect), [`Html<T>`](utils::Html) and [`CookieJar`](utils::CookieJar)
//! - [`mod@openapi`] — Automatic `OpenAPI` documentation from annotated handlers
//! - [`runtime`] — Runtime primitives for `#[skyzen::main]`, including the platform detection above
//! - [`static_files`] — Streamed file serving with `ETag`, `Range` and SPA fallback (requires
//!   `static-files`)
//! - [`websocket`] — Unified WebSocket API across native and WASM (requires `ws` feature)
//!
//! # Safe by default
//!
//! - A `5xx` response body is redacted to `"Internal server error"`; the real message and its whole
//!   `source()` chain go to the log. A `4xx` message is returned verbatim, because it is about the
//!   caller's request.
//! - Request bodies are capped at [`RequestBodyLimit::DEFAULT`] (2 MiB) with no configuration,
//!   enforced from `Content-Length` *and* mid-stream.
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
    sql, tail, test, Column, FromRow, HttpError,
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
/// Service types used by macro expansions.
///
/// Keeping this path behind the root crate means applications using manifest-driven wiring do
/// not need to declare Skyzen's implementation dependency themselves.
#[doc(hidden)]
pub use skyzen_services as __services;

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
