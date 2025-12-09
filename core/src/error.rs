//! A module defining the main error type and related utilities for HTTP operations.

use core::ops::{Deref, DerefMut};

use alloc::boxed::Box;
use http_kit::{HttpError, StatusCode};

/// A specialized `Result` type for HTTP operations.
/// This is a convenient alias for `core::result::Result<T, Error>`,
/// where `Error` is the main error type defined in this module.
pub type Result<T> = core::result::Result<T, Error>;

/// The main error type for HTTP operations.
///
/// This error type wraps any error with an associated HTTP status code,
/// providing both the underlying error information and the appropriate
/// HTTP response status.
///
/// # Examples
///
/// ```rust
/// # use skyzen_core::Error;
/// #  use skyzen_core::StatusCode;
///
/// // Create from a string message
/// let err = Error::msg("Something went wrong");
///
/// // Create with a specific status code
/// let err = Error::msg("Not found").set_status(StatusCode::NOT_FOUND);
/// ```
pub struct Error {
    error: eyre::Error,
    status: StatusCode,
}

impl From<Error> for Box<dyn HttpError> {
    fn from(error: Error) -> Self {
        #[derive(Debug)]
        struct Wrapper {
            inner: Error,
        }

        impl core::error::Error for Wrapper {}

        impl core::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "{}", self.inner)
            }
        }

        impl HttpError for Wrapper {
            fn status(&self) -> StatusCode {
                self.inner.status()
            }
        }

        Box::new(Wrapper { inner: error })
    }
}

impl Error {
    /// Creates a new `Error` from any error type with the given HTTP status code.
    ///
    /// # Arguments
    ///
    /// * `error` - Any error type that can be converted to `anyhow::Error`
    /// * `status` - HTTP status code (or value convertible to one)
    ///
    /// # Panics
    ///
    /// Panics if the status code is invalid.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use skyzen_core::Error;
    /// # use skyzen_core::StatusCode;
    ///
    /// let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    /// let http_err = Error::new(io_err, StatusCode::NOT_FOUND);
    /// ```
    pub fn new<E, S>(error: E, status: S) -> Self
    where
        E: Into<eyre::Error>,
        S: TryInto<StatusCode>,
        S::Error: core::fmt::Debug,
    {
        Self {
            error: error.into(),
            status: status.try_into().unwrap(), //may panic if user delivers an illegal code.
        }
    }

    /// Creates an `Error` from a message string with a default status code.
    ///
    /// The default status code is `SERVICE_UNAVAILABLE` (503).
    ///
    /// # Arguments
    ///
    /// * `msg` - Any type that implements `Display + Debug + Send + 'static`
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use skyzen_core::Error;
    ///
    /// let err = Error::msg("Something went wrong");
    /// let err = Error::msg(format!("Failed to process item {}", 42));
    /// ```
    pub fn msg<S>(msg: S) -> Self
    where
        S: core::fmt::Display + core::fmt::Debug + Send + Sync + 'static,
    {
        Self {
            error: eyre::Error::msg(msg),
            status: StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// Sets the HTTP status code of this error.
    ///
    /// Only error status codes (400-599) can be set. In debug builds,
    /// this method will assert that the status code is in the valid range.
    ///
    /// # Arguments
    ///
    /// * `status` - HTTP status code (or value convertible to one)
    ///
    /// # Panics
    ///
    /// Panics if the status code is invalid or not an error status code.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// # use skyzen_core::Error;
    /// # use skyzen_core::StatusCode;
    ///
    /// let err = Error::msg("Not found").set_status(StatusCode::NOT_FOUND);
    /// ```
    #[must_use]
    pub fn set_status<S>(mut self, status: S) -> Self
    where
        S: TryInto<StatusCode>,
        S::Error: core::fmt::Debug,
    {
        let status = status.try_into().expect("Invalid status code");
        if cfg!(debug_assertions) {
            assert!(
                (400..=599).contains(&status.as_u16()),
                "Expected a status code within 400~599"
            );
        }

        self.status = status;

        self
    }

    /// Returns the HTTP status code associated with this error.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// # use skyzen_core::Error;
    /// # use skyzen_core::StatusCode;
    ///
    /// let err = Error::msg("not found").set_status(StatusCode::NOT_FOUND);
    /// assert_eq!(err.status(), StatusCode::NOT_FOUND);
    /// ```
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Attempts to downcast the inner error to a concrete type.
    ///
    /// Returns `Ok(Box<E>)` if the downcast succeeds, or `Err(Self)` if it fails.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// # use skyzen_core::Error;
    /// # use skyzen_core::StatusCode;
    /// use std::io;
    ///
    /// let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
    /// let http_err = Error::new(io_err, StatusCode::NOT_FOUND);
    ///
    /// match http_err.downcast::<io::Error>() {
    ///     Ok(io_error) => println!("Got IO error: {}", io_error),
    ///     Err(original) => println!("Not an IO error: {}", original),
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns `Err(Self)` when the inner error cannot be downcast into the requested type.
    pub fn downcast<E>(self) -> core::result::Result<Box<E>, Self>
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        let Self { status, error } = self;
        error.downcast().map_err(|error| Self { error, status })
    }

    /// Attempts to downcast the inner error to a reference of the concrete type.
    ///
    /// Returns `Some(&E)` if the downcast succeeds, or `None` if it fails.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// # use skyzen_core::Error;
    /// # use skyzen_core::StatusCode;
    /// use std::io;
    ///
    /// let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
    /// let http_err = Error::new(io_err, StatusCode::NOT_FOUND);
    ///
    /// if let Some(io_error) = http_err.downcast_ref::<io::Error>() {
    ///     println!("IO error kind: {:?}", io_error.kind());
    /// }
    /// ```
    #[must_use]
    pub fn downcast_ref<E>(&self) -> Option<&E>
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        self.error.downcast_ref()
    }

    /// Attempts to downcast the inner error to a mutable reference of the concrete type.
    ///
    /// Returns `Some(&mut E)` if the downcast succeeds, or `None` if it fails.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// # use skyzen_core::Error;
    /// # use skyzen_core::StatusCode;
    /// use std::io;
    ///
    /// let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
    /// let mut http_err = Error::new(io_err, StatusCode::NOT_FOUND);
    ///
    /// if let Some(io_error) = http_err.downcast_mut::<io::Error>() {
    ///     // Modify the IO error if needed
    /// }
    /// ```
    pub fn downcast_mut<E>(&mut self) -> Option<&mut E>
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        self.error.downcast_mut()
    }

    /// Converts this error into a boxed standard error trait object.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// # use skyzen_core::Error;
    /// let err = Error::msg("Not found");
    /// let boxed_err: Box<dyn std::error::Error + Send> = err.into_boxed_error();
    /// ```
    #[must_use]
    pub fn into_boxed_error(self) -> Box<dyn core::error::Error + Send + 'static> {
        self.into_boxed_http_error() as Box<dyn core::error::Error + Send + 'static>
    }

    /// Converts this error into a boxed `HttpError` trait object.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// # use skyzen_core::Error;
    /// # use skyzen_core::StatusCode;
    /// let err = Error::msg("Not found").set_status(StatusCode::NOT_FOUND);
    /// let boxed_err: Box<dyn skyzen::HttpError> = err.into_boxed_http_error();
    /// ```
    #[must_use]
    pub fn into_boxed_http_error(self) -> Box<dyn HttpError> {
        struct Wrapper {
            inner: Error,
        }

        impl core::error::Error for Wrapper {}
        impl core::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "{}", self.inner)
            }
        }
        impl core::fmt::Debug for Wrapper {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::Debug::fmt(&self.inner, f)
            }
        }
        impl HttpError for Wrapper {
            fn status(&self) -> StatusCode {
                self.inner.status()
            }
        }
        Box::new(Wrapper { inner: self })
    }
}

impl<E: core::error::Error + Send + Sync + 'static> From<E> for Error {
    fn from(error: E) -> Self {
        Self::new(error, StatusCode::SERVICE_UNAVAILABLE)
    }
}

impl From<Error> for Box<dyn core::error::Error> {
    fn from(error: Error) -> Self {
        error.error.into()
    }
}

impl core::fmt::Debug for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&self.error, f)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.error, f)
    }
}

impl AsRef<dyn core::error::Error + Send + 'static> for Error {
    fn as_ref(&self) -> &(dyn core::error::Error + Send + 'static) {
        &**self
    }
}

impl AsMut<dyn core::error::Error + Send + 'static> for Error {
    fn as_mut(&mut self) -> &mut (dyn core::error::Error + Send + 'static) {
        &mut **self
    }
}

impl Deref for Error {
    type Target = dyn core::error::Error + Send + 'static;

    fn deref(&self) -> &Self::Target {
        &*self.error
    }
}

impl DerefMut for Error {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.error
    }
}

/// Extension trait that adds HTTP status code handling to `Result` and `Option` types.
///
/// This trait provides a convenient `status` method that allows you to associate
/// an HTTP status code with errors when converting them to the HTTP toolkit's
/// `Result` type.
///
/// # Examples
///
/// ```rust,ignore
/// use skyzen::{ResultExt, Result};
/// # use skyzen_core::StatusCode;
/// use std::fs;
///
/// fn read_config() -> Result<String> {
///     fs::read_to_string("config.txt")
///         .status(StatusCode::NOT_FOUND)
/// }
///
/// fn get_user_id() -> Result<u32> {
///     Some(42_u32)
///         .status(StatusCode::BAD_REQUEST)
/// }
/// ```
pub trait ResultExt<T>
where
    Self: Sized,
{
    /// Associates an HTTP status code with an error or None value.
    ///
    /// For `Result` types, this wraps any error with the specified status code.
    /// For `Option` types, this converts `None` to an error with the specified status code.
    ///
    /// # Arguments
    ///
    /// * `status` - HTTP status code to associate with the error
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use skyzen::{ResultExt, Result};
    /// # use skyzen_core::StatusCode;
    /// use std::fs;
    ///
    /// // With Result
    /// let result: Result<String> = fs::read_to_string("missing.txt")
    ///     .status(StatusCode::NOT_FOUND);
    ///
    /// // With Option
    /// let result: Result<i32> = None
    ///     .status(StatusCode::BAD_REQUEST);
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an `Error` when the result is `Err` or the option is `None`, using the provided status code.
    fn status<S>(self, status: S) -> Result<T>
    where
        S: TryInto<StatusCode>,
        S::Error: core::fmt::Debug;
}

impl<T, E> ResultExt<T> for core::result::Result<T, E>
where
    E: core::error::Error + Send + Sync + 'static,
{
    fn status<S>(self, status: S) -> Result<T>
    where
        S: TryInto<StatusCode>,
        S::Error: core::fmt::Debug,
    {
        self.map_err(|error| Error::new(error, status))
    }
}

impl<T> ResultExt<T> for Option<T> {
    fn status<S>(self, status: S) -> Result<T>
    where
        S: TryInto<StatusCode>,
        S::Error: core::fmt::Debug,
    {
        self.ok_or_else(|| Error::msg("None Error").set_status(status))
    }
}
