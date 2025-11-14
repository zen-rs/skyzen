#![deny(unsafe_code)]
#![no_std]
//! Base type and trait for HTTP server.

extern crate alloc;

#[macro_use]
mod macros;

mod extract;
pub use extract::Extractor;
mod responder;
pub use responder::Responder;
mod server;
pub use server::Server;
