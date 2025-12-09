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
#[cfg(feature = "openapi")]
pub mod openapi;

pub use http_kit::{
    endpoint, header, method, middleware, uri, version, Body, BodyError, Endpoint, Extensions,
    Method, Middleware, Request, Response, Result, ResultExt, StatusCode, Uri, Version,
};

/// Error types used in skyzen.
pub mod error {
    use std::fmt::{Debug, Display};

    // Since `error[E0119]`, we have to wrap `http-kit`'s `Error` here.
    pub use http_kit::error::{BoxHttpError, HttpError};

    use http_kit::{Error as HttpKitError, StatusCode};

    /// A concrete error type for HTTP operations.
    pub struct Error(http_kit::error::Error);

    impl Debug for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            Debug::fmt(&self.0, f)
        }
    }

    impl Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            Display::fmt(&self.0, f)
        }
    }

    impl Error {
        /// Create a new error from any standard error type.
        pub fn new(e: impl Into<eyre::Report>) -> Self {
            Self(HttpKitError::new(e))
        }

        /// Create a new error with a custom message.
        pub fn msg(msg: impl Display + Send + Sync + Debug + 'static) -> Self {
            Self(HttpKitError::msg(msg))
        }

        /// Consume the error and return the inner `eyre::Report`.
        pub fn into_inner(self) -> eyre::Report {
            self.0.into_inner()
        }

        /// Convert this error into a boxed HTTP error trait object.
        #[must_use]
        pub fn into_boxed_http_error(self) -> BoxHttpError {
            self.0.into_boxed_http_error()
        }

        /// Set the HTTP status code for this error.
        #[must_use]
        pub fn set_status(self, status: StatusCode) -> Self {
            Self(self.0.set_status(status))
        }
    }

    impl<T> From<T> for Error
    where
        T: Into<HttpKitError>,
    {
        fn from(value: T) -> Self {
            Self(value.into())
        }
    }

    /// Result type used in skyzen.
    pub type Result<T> = core::result::Result<T, Error>;

    /// Extension trait for `Result` and `Option` to set HTTP status code on error.
    #[allow(clippy::missing_errors_doc)]
    pub trait ResultExt<T> {
        /// Set the HTTP status code for this error.
        fn status(self, status: StatusCode) -> Result<T>;
    }

    impl<T, E: Into<Error>> ResultExt<T> for core::result::Result<T, E> {
        fn status(self, status: StatusCode) -> Result<T> {
            self.map_err(|e| e.into().set_status(status))
        }
    }

    impl<T> ResultExt<T> for core::option::Option<T> {
        fn status(self, status: StatusCode) -> Result<T> {
            self.ok_or_else(|| Error::msg("None").set_status(status))
        }
    }
}

pub use error::*;
