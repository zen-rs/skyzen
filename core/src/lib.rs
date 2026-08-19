#![deny(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]
//! Foundational traits and types for the Skyzen HTTP framework.
//!
//! This crate defines the core abstractions that all Skyzen components build upon:
//!
//! - [`Extractor`] — Pull typed data from HTTP requests
//! - [`Responder`] — Convert types into HTTP responses
//! - [`Server`] — HTTP server backend trait (implemented by `skyzen-hyper`)
//!
//! Also re-exports HTTP primitives from `http-kit`: [`Request`], [`Response`],
//! [`Body`], [`Endpoint`], [`Middleware`], [`StatusCode`], and more.
//!
//! # `no_std` Support
//!
//! Disable the default `std` feature for `no_std` environments:
//!
//! ```toml
//! skyzen-core = { version = "0.1", default-features = false }
//! ```
//!
//! Most users should use the `skyzen` crate directly rather than depending on
//! `skyzen-core`.

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
#[cfg(feature = "std")]
mod net;
#[cfg(feature = "std")]
pub use net::{error_response, MissingRemoteAddr, PeerAddr};
#[cfg(feature = "openapi")]
pub mod openapi;

pub use http_kit::{
    endpoint, header, method, middleware, uri, version, Body, BodyError, Endpoint, Extensions,
    Method, Middleware, Request, Response, StatusCode, Uri, Version,
};

/// Error types used in skyzen.
pub mod error {
    use core::fmt::{Debug, Display};

    // Since `error[E0119]`, we have to wrap `http-kit`'s `Error` here.
    pub use http_kit::error::{BoxHttpError, HttpError};

    use http_kit::{Error as HttpKitError, StatusCode};

    /// A concrete error type for HTTP operations.
    pub struct Error(http_kit::error::Error);

    impl Debug for Error {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Debug::fmt(&self.0, f)
        }
    }

    impl Display for Error {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Display::fmt(&self.0, f)
        }
    }

    impl Error {
        /// Create a new error from any standard error type.
        ///
        /// Requires the `std` feature because the conversion goes through
        /// [`eyre::Report`].
        #[cfg(feature = "std")]
        pub fn new(e: impl Into<eyre::Report>) -> Self {
            Self(HttpKitError::new(e))
        }

        /// Create a new error with a custom message.
        pub fn msg(msg: impl Display + Send + Sync + Debug + 'static) -> Self {
            Self(HttpKitError::msg(msg))
        }

        /// Consume the error and return the inner `eyre::Report`.
        ///
        /// Requires the `std` feature because [`eyre::Report`] itself does.
        #[cfg(feature = "std")]
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

pub use error::{BoxHttpError, Error, HttpError, Result, ResultExt};
