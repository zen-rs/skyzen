#![deny(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

//! Base type and trait for HTTP server.

#[macro_use]
mod macros;

mod extract;
pub use extract::Extractor;
mod responder;
pub use responder::Responder;