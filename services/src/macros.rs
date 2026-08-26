//! Internal macros shared by the type-erased service wrapper types.

/// Generate the constructors for a service error's `Backend { message, source }` variant.
///
/// Every service error enum carries the same backend variant, so the two constructors that build
/// it — with and without an underlying cause — are emitted once here instead of being repeated
/// per enum.
macro_rules! backend_error {
    ($error:ident) => {
        impl $error {
            /// Build a backend error from a message, with no underlying cause to attach.
            ///
            /// Prefer [`Self::backend_with`] whenever the backend hands back a real error value,
            /// so the source chain survives into the logs.
            pub fn backend(message: impl Into<::std::string::String>) -> Self {
                Self::Backend {
                    message: message.into(),
                    source: None,
                }
            }

            /// Build a backend error from a message and the underlying backend error.
            pub fn backend_with(
                message: impl Into<::std::string::String>,
                source: impl Into<$crate::BoxError>,
            ) -> Self {
                Self::Backend {
                    message: message.into(),
                    source: ::core::option::Option::Some(source.into()),
                }
            }
        }
    };
}

/// Implement [`http_kit::HttpError`] for a service error by listing the status of each variant.
///
/// Keeping the mapping in one shape makes the per-service policy readable at a glance and stops
/// the seven enums from drifting apart.
macro_rules! service_http_error {
    ($error:ident { $($pattern:pat => $status:ident),+ $(,)? }) => {
        impl ::http_kit::HttpError for $error {
            fn status(&self) -> ::http_kit::StatusCode {
                match self {
                    $($pattern => ::http_kit::StatusCode::$status,)+
                }
            }
        }
    };
}

/// Generate the boilerplate plumbing shared by every type-erased service wrapper.
///
/// Given a newtype wrapper whose single field is a `Box<dyn _Obj>` exposing `clone_box`, this emits:
/// - `Clone` (cloning via `clone_box`),
/// - `Debug` (opaque, `finish_non_exhaustive`),
/// - the `*NotConfigured` extractor error (HTTP 500),
/// - the [`Extractor`](skyzen_core::Extractor) impl that reads the wrapper from request extensions,
/// - the [`Middleware`](http_kit::middleware::Middleware) impl that injects the wrapper into them.
///
/// Centralising this keeps the seven service wrappers byte-for-byte consistent and means a change to
/// the injection/extraction contract is made in exactly one place.
macro_rules! service_extractor {
    ($wrapper:ident, $not_configured:ident, $message:literal $(,)?) => {
        impl ::core::clone::Clone for $wrapper {
            fn clone(&self) -> Self {
                Self(self.0.clone_box())
            }
        }

        impl ::core::fmt::Debug for $wrapper {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.debug_struct(stringify!($wrapper)).finish_non_exhaustive()
            }
        }

        ::http_kit::http_error!(
            #[doc = $message]
            pub $not_configured,
            ::http_kit::StatusCode::INTERNAL_SERVER_ERROR,
            $message
        );

        impl ::skyzen_core::Extractor for $wrapper {
            type Error = $not_configured;
            async fn extract(
                request: &mut ::http_kit::Request,
            ) -> ::core::result::Result<Self, Self::Error> {
                request
                    .extensions()
                    .get::<Self>()
                    .cloned()
                    .ok_or($not_configured::new())
            }
        }

        impl ::http_kit::middleware::Middleware for $wrapper {
            type Error = ::core::convert::Infallible;
            async fn handle<N: ::http_kit::Endpoint>(
                &mut self,
                request: &mut ::http_kit::Request,
                mut next: N,
            ) -> ::core::result::Result<
                ::http_kit::Response,
                ::http_kit::middleware::MiddlewareError<N::Error, Self::Error>,
            > {
                request.extensions_mut().insert(self.clone());
                next.respond(request)
                    .await
                    .map_err(::http_kit::middleware::MiddlewareError::Endpoint)
            }
        }
    };
}
