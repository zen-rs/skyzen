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
    use alloc::boxed::Box;
    use core::error::Error as StdError;
    use core::fmt::{Debug, Display};

    pub use http_kit::error::{BoxHttpError, HttpError};

    use http_kit::{Error as HttpKitError, StatusCode};

    /// A dynamically typed error carrying an HTTP status code.
    ///
    /// `Error` is the error half of [`Result`], the return type Skyzen handlers use when they
    /// mix several failure modes. Converting into it with `?` preserves the status of any
    /// [`HttpError`], so a `400` rejection stays a `400` all the way to the response.
    ///
    /// Errors that carry no HTTP meaning of their own do not convert implicitly; wrap them with
    /// [`Error::new`], [`Error::msg`] or [`ResultExt::status`] to state the status explicitly.
    pub struct Error {
        inner: BoxHttpError,
        status: StatusCode,
    }

    impl Debug for Error {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Error")
                .field("status", &self.status.as_u16())
                .field("source", &self.inner)
                .finish()
        }
    }

    impl Display for Error {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Display::fmt(&self.inner, f)
        }
    }

    impl StdError for Error {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.inner.as_ref().source()
        }
    }

    impl Error {
        /// Create a new error from any standard error type.
        ///
        /// The resulting error reports `500 Internal Server Error`. Use [`Error::set_status`] or
        /// [`ResultExt::status`] to attach a more specific status; errors that already implement
        /// [`HttpError`] should go through `?`/[`From`] instead, which keeps their own status.
        pub fn new(error: impl StdError + Send + Sync + 'static) -> Self {
            Self {
                inner: Box::new(Adapter(error)),
                status: StatusCode::INTERNAL_SERVER_ERROR,
            }
        }

        /// Create a new error with a custom message.
        ///
        /// The resulting error reports `500 Internal Server Error`.
        pub fn msg(msg: impl Display + Send + Sync + Debug + 'static) -> Self {
            Self {
                inner: Box::new(Message(msg)),
                status: StatusCode::INTERNAL_SERVER_ERROR,
            }
        }

        /// Convert an [`http_kit::Error`] into a Skyzen error, preserving its status code.
        ///
        /// http-kit's error type deliberately does not implement [`HttpError`], so the blanket
        /// [`From`] conversion cannot cover it and this constructor takes its place.
        #[must_use]
        pub fn from_http_kit(error: HttpKitError) -> Self {
            let inner = error.into_boxed_http_error();
            let status = inner.status();
            Self { inner, status }
        }

        /// The HTTP status code this error will be rendered with.
        #[must_use]
        pub const fn status(&self) -> StatusCode {
            self.status
        }

        /// Convert this error into a boxed HTTP error trait object.
        #[must_use]
        pub fn into_boxed_http_error(self) -> BoxHttpError {
            if self.inner.status() == self.status {
                self.inner
            } else {
                Box::new(StatusOverride {
                    inner: self.inner,
                    status: self.status,
                })
            }
        }

        /// Set the HTTP status code for this error.
        #[must_use]
        pub const fn set_status(mut self, status: StatusCode) -> Self {
            self.status = status;
            self
        }

        /// Add a human-readable breadcrumb describing what failed.
        ///
        /// The message becomes the error's [`Display`] rendering and the previous error becomes
        /// its [`source`](StdError::source), so the whole chain reaches the logs. The status code
        /// is carried over unchanged.
        #[must_use]
        pub fn context(self, msg: impl Display + Send + Sync + Debug + 'static) -> Self {
            let status = self.status;
            Self {
                inner: Box::new(Contextual {
                    message: msg,
                    source: self,
                }),
                status,
            }
        }
    }

    /// Wraps a plain standard error so it can be stored as a [`BoxHttpError`].
    struct Adapter<E>(E);

    impl<E: Debug> Debug for Adapter<E> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Debug::fmt(&self.0, f)
        }
    }

    impl<E: Display> Display for Adapter<E> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Display::fmt(&self.0, f)
        }
    }

    impl<E: StdError> StdError for Adapter<E> {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.0.source()
        }
    }

    impl<E: StdError + Send + Sync + 'static> HttpError for Adapter<E> {}

    /// Wraps a bare message so it can be stored as a [`BoxHttpError`].
    struct Message<M>(M);

    impl<M: Debug> Debug for Message<M> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Debug::fmt(&self.0, f)
        }
    }

    impl<M: Display> Display for Message<M> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Display::fmt(&self.0, f)
        }
    }

    impl<M: Debug + Display> StdError for Message<M> {}

    impl<M: Debug + Display + Send + Sync + 'static> HttpError for Message<M> {}

    /// A breadcrumb message layered on top of an existing [`Error`].
    struct Contextual<M> {
        message: M,
        source: Error,
    }

    impl<M: Debug> Debug for Contextual<M> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Context")
                .field("message", &self.message)
                .field("source", &self.source)
                .finish()
        }
    }

    impl<M: Display> Display for Contextual<M> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Display::fmt(&self.message, f)
        }
    }

    impl<M: Debug + Display> StdError for Contextual<M> {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(&self.source)
        }
    }

    impl<M: Debug + Display + Send + Sync + 'static> HttpError for Contextual<M> {
        fn status(&self) -> StatusCode {
            self.source.status
        }
    }

    /// Re-states the status of an already boxed HTTP error.
    struct StatusOverride {
        inner: BoxHttpError,
        status: StatusCode,
    }

    impl Debug for StatusOverride {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Debug::fmt(&self.inner, f)
        }
    }

    impl Display for StatusOverride {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            Display::fmt(&self.inner, f)
        }
    }

    impl StdError for StatusOverride {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.inner.as_ref().source()
        }
    }

    impl HttpError for StatusOverride {
        fn status(&self) -> StatusCode {
            self.status
        }
    }

    impl<E: HttpError> From<E> for Error {
        fn from(error: E) -> Self {
            let status = error.status();
            Self {
                inner: Box::new(error),
                status,
            }
        }
    }

    /// Result type used in skyzen.
    pub type Result<T> = core::result::Result<T, Error>;

    /// Extension trait for `Result` and `Option` to set HTTP status code on error.
    #[allow(clippy::missing_errors_doc)]
    pub trait ResultExt<T> {
        /// Convert the error into an [`Error`] carrying the given status code.
        fn status(self, status: StatusCode) -> Result<T>;

        /// Convert the error into an [`Error`] carrying the given status code and message.
        ///
        /// The original error is kept as the [`source`](StdError::source) of the returned error,
        /// so operators still see it while clients only see `msg` (for 4xx statuses).
        fn status_msg(
            self,
            status: StatusCode,
            msg: impl Display + Send + Sync + Debug + 'static,
        ) -> Result<T>;
    }

    impl<T, E> ResultExt<T> for core::result::Result<T, E>
    where
        E: StdError + Send + Sync + 'static,
    {
        fn status(self, status: StatusCode) -> Result<T> {
            self.map_err(|error| Error::new(error).set_status(status))
        }

        fn status_msg(
            self,
            status: StatusCode,
            msg: impl Display + Send + Sync + Debug + 'static,
        ) -> Result<T> {
            self.map_err(|error| Error::new(error).context(msg).set_status(status))
        }
    }

    impl<T> ResultExt<T> for core::option::Option<T> {
        fn status(self, status: StatusCode) -> Result<T> {
            self.ok_or_else(|| {
                status
                    .canonical_reason()
                    .map_or_else(|| Error::msg(status), Error::msg)
                    .set_status(status)
            })
        }

        fn status_msg(
            self,
            status: StatusCode,
            msg: impl Display + Send + Sync + Debug + 'static,
        ) -> Result<T> {
            self.ok_or_else(|| Error::msg(msg).set_status(status))
        }
    }

    /// Extension trait for attaching a breadcrumb message to a failing operation.
    ///
    /// Only errors that already carry an HTTP status (or an [`Error`]) can be given context
    /// directly; for anything else state the status first with [`ResultExt::status`].
    #[allow(clippy::missing_errors_doc)]
    pub trait Context<T> {
        /// Attach a message describing what the failing operation was trying to do.
        fn context(self, msg: impl Display + Send + Sync + Debug + 'static) -> Result<T>;
    }

    impl<T, E: Into<Error>> Context<T> for core::result::Result<T, E> {
        fn context(self, msg: impl Display + Send + Sync + Debug + 'static) -> Result<T> {
            self.map_err(|error| error.into().context(msg))
        }
    }

    impl<T> Context<T> for core::option::Option<T> {
        fn context(self, msg: impl Display + Send + Sync + Debug + 'static) -> Result<T> {
            self.ok_or_else(|| Error::msg(msg))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{Context, Error, HttpError, Result, ResultExt, StdError};
        use http_kit::{http_error, StatusCode};

        http_error!(
            /// A client rejection carrying a 400 status.
            BadInput,
            StatusCode::BAD_REQUEST,
            "Missing param `id`"
        );

        #[derive(Debug)]
        struct PlainError;

        impl core::fmt::Display for PlainError {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("plain failure")
            }
        }

        impl StdError for PlainError {}

        fn bubble() -> Result<()> {
            Err(BadInput::new())?;
            Ok(())
        }

        #[test]
        fn question_mark_preserves_http_status() {
            let error = bubble().unwrap_err();
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
            assert_eq!(error.to_string(), "Missing param `id`");
        }

        #[test]
        fn boxed_conversion_keeps_status() {
            let boxed = bubble().unwrap_err().into_boxed_http_error();
            assert_eq!(boxed.status(), StatusCode::BAD_REQUEST);
            assert_eq!(boxed.to_string(), "Missing param `id`");
        }

        #[test]
        fn msg_defaults_to_internal_server_error() {
            assert_eq!(
                Error::msg("boom").status(),
                StatusCode::INTERNAL_SERVER_ERROR
            );
        }

        #[test]
        fn new_wraps_plain_errors_as_server_errors() {
            let error = Error::new(PlainError);
            assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(error.to_string(), "plain failure");
        }

        #[test]
        fn context_keeps_status_and_chains_source() {
            let error = bubble().unwrap_err().context("loading user");
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
            assert_eq!(error.to_string(), "loading user");
            let source = StdError::source(&error).expect("context keeps a source");
            assert_eq!(source.to_string(), "Missing param `id`");
        }

        #[test]
        fn result_context_preserves_status() {
            let error = core::result::Result::<(), _>::Err(BadInput::new())
                .context("while parsing")
                .unwrap_err();
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
            assert_eq!(error.to_string(), "while parsing");
        }

        #[test]
        fn result_status_overrides_plain_errors() {
            let error = core::result::Result::<(), _>::Err(PlainError)
                .status(StatusCode::NOT_FOUND)
                .unwrap_err();
            assert_eq!(error.status(), StatusCode::NOT_FOUND);
            assert_eq!(error.to_string(), "plain failure");
        }

        #[test]
        fn result_status_msg_hides_source_behind_message() {
            let error = core::result::Result::<(), _>::Err(PlainError)
                .status_msg(StatusCode::NOT_FOUND, "user not found")
                .unwrap_err();
            assert_eq!(error.status(), StatusCode::NOT_FOUND);
            assert_eq!(error.to_string(), "user not found");
            assert_eq!(
                StdError::source(&error)
                    .expect("original error is kept as source")
                    .to_string(),
                "plain failure"
            );
        }

        #[test]
        fn option_status_uses_canonical_reason() {
            let error = None::<()>.status(StatusCode::NOT_FOUND).unwrap_err();
            assert_eq!(error.status(), StatusCode::NOT_FOUND);
            assert_eq!(error.to_string(), "Not Found");
        }

        #[test]
        fn option_status_msg_uses_supplied_message() {
            let error = None::<()>
                .status_msg(StatusCode::NOT_FOUND, "no such user")
                .unwrap_err();
            assert_eq!(error.status(), StatusCode::NOT_FOUND);
            assert_eq!(error.to_string(), "no such user");
        }
    }
}

pub use error::{BoxHttpError, Context, Error, HttpError, Result, ResultExt};
