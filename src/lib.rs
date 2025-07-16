#![deny(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

//! A simple and fast web server framework.

#[macro_use]
mod macros;

/*#[cfg(test)]
#[macro_use]
mod test_helper;*/

pub mod handler;

//pub mod routing;

/// Utilities.
pub mod utils;

#[doc(inline)]
pub use http_kit::{
    header, Body, Endpoint, Error, Method, Middleware, Request, Response, Result, ResultExt,
    StatusCode, Uri,
};
pub use skyzen_core::Server;
//#[doc(inline)]
//pub use routing::{CreateRouteNode, Route};

/// Extract strong-typed object from your request.
pub mod extract;

pub mod responder;
pub use responder::Responder;

pub mod middleware;
