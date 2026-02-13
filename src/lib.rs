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
pub use skyzen_macros::{error, import_config, main, openapi, HttpError};

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
#[doc(inline)]
pub use routing::{CreateRouteNode, Route};
pub use skyzen_core::error::*;
pub use skyzen_core::Server;
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
