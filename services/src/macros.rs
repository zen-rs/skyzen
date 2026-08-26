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

/// Generate a service's object-safe `*Obj` trait and its blanket bridge from the public trait's
/// method signatures.
///
/// Every service is defined twice: once as an ergonomic public trait returning `impl Future`, and
/// once as an object-safe mirror returning [`BoxFuture`](crate::BoxFuture) so the wrapper can hold
/// a `Box<dyn _>`. The mirror and the bridge that forwards to the public trait are derivable from
/// the public signatures alone, so they are generated here instead of being written out seven
/// times — adding a method to a service is then a single-site change.
///
/// Methods are listed in the order the trait declares them, synchronous ones first:
///
/// ```ignore
/// service_obj! {
///     KeyValueStoreObj: KeyValueStore;
///     async fn get<'a>(&'a self, key: &'a str) -> Result<Option<Vec<u8>>, KvError>;
///     async fn delete_all(&'_ self) -> Result<(), KvError>;
/// }
/// ```
///
/// Write the signature exactly as the object-safe trait needs it: a named `<'a>` binds every
/// borrowed argument to the borrow of `self`, which is what lets the boxed future hold them, and
/// `&'_ self` marks a method whose future borrows nothing else. The return type is the public
/// trait's `Output`; the macro wraps it in `BoxFuture` carrying that same `self` lifetime.
macro_rules! service_obj {
    (
        $(#[$meta:meta])*
        $obj:ident: $public:ident;
        $(
            fn $sync_method:ident(&self $(, $sync_arg:ident: $sync_arg_ty:ty)* $(,)?)
                -> $sync_ret:ty;
        )*
        $(
            async fn $method:ident $(<$lifetime:lifetime>)? (
                & $self_lifetime:lifetime self
                $(, $arg:ident: $arg_ty:ty)* $(,)?
            ) -> $ret:ty;
        )+
    ) => {
        $(#[$meta])*
        trait $obj: Send + Sync {
            $(
                fn $sync_method(&self $(, $sync_arg: $sync_arg_ty)*) -> $sync_ret;
            )*
            $(
                fn $method $(<$lifetime>)? (
                    & $self_lifetime self
                    $(, $arg: $arg_ty)*
                ) -> $crate::BoxFuture<$self_lifetime, $ret>;
            )+
            /// Clone the concrete backend that sits behind this trait object.
            fn clone_box(&self) -> ::std::boxed::Box<dyn $obj>;
        }

        impl<T: $public> $obj for T {
            $(
                fn $sync_method(&self $(, $sync_arg: $sync_arg_ty)*) -> $sync_ret {
                    <Self as $public>::$sync_method(self $(, $sync_arg)*)
                }
            )*
            $(
                fn $method $(<$lifetime>)? (
                    & $self_lifetime self
                    $(, $arg: $arg_ty)*
                ) -> $crate::BoxFuture<$self_lifetime, $ret> {
                    ::std::boxed::Box::pin(<Self as $public>::$method(self $(, $arg)*))
                }
            )+

            fn clone_box(&self) -> ::std::boxed::Box<dyn $obj> {
                ::std::boxed::Box::new(::core::clone::Clone::clone(self))
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
/// - the [`Middleware`](skyzen_core::middleware::Middleware) impl that injects the wrapper into
///   them, declaring the wrapper as one of its `provisions()`.
///
/// Centralising this keeps the seven service wrappers byte-for-byte consistent and means a change to
/// the injection/extraction contract is made in exactly one place.
///
/// The `Extractor` impl deliberately declares **no** `requirements()`, so `Route::try_build` never
/// rejects a route for a service it cannot see. Route middleware is not the only way a wrapper
/// reaches a request: `#[skyzen::main]` wraps the built router with the services declared in
/// `Skyzen.toml`, and `skyzen_test::TestClient` inserts them straight into the extensions. Either
/// path would turn a correctly wired application into a build-time panic.
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

        impl ::skyzen_core::middleware::Middleware for $wrapper {
            async fn handle(
                &self,
                request: &mut ::http_kit::Request,
                next: ::skyzen_core::middleware::Next<'_>,
            ) -> ::core::result::Result<::http_kit::Response, ::skyzen_core::Error> {
                request.extensions_mut().insert(self.clone());
                next.run(request).await
            }

            fn provisions(&self) -> ::std::vec::Vec<::core::any::TypeId> {
                ::std::vec![::core::any::TypeId::of::<Self>()]
            }
        }
    };
}
