#![deny(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]
//! Base type and trait for HTTP server.

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

#[macro_use]
mod macros;

mod extract;
pub use extract::Extractor;
mod responder;
pub use responder::Responder;
mod server;
pub use server::Server;
pub mod error;
pub use error::*;
#[cfg(feature = "openapi")]
pub mod openapi;

pub use http_kit::{
    endpoint, header, method, middleware, uri, version, Body, BodyError, Endpoint, Extensions,
    Method, Middleware, Request, Response, Result, ResultExt, StatusCode, Uri, Version,
};
