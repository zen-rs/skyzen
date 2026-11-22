//! Handle the request and make a response.
//! ```rust
//! // A simple echo server
//! async fn handler(body: String) -> http_kit::Result<String> {
//!     Ok(body)
//! }
//! ```

use core::{future::Future, marker::PhantomData};
use http_kit::{Endpoint, Request, Response};
use skyzen_core::{Extractor, Responder};
use std::fmt::Display;

/// Error type for handler operations.
pub enum HandlerError<E: Extractor, R: Responder> {
    /// An error occurred during extraction.
    ExtractorError(E::Error),
    /// An error occurred during response generation.
    ResponderError(R::Error),
}

impl<E: Extractor, R: Responder> Display for HandlerError<E, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ExtractorError(e) => write!(f, "{e}"),
            Self::ResponderError(e) => write!(f, "{e}"),
        }
    }
}

impl<E: Extractor, R: Responder> core::fmt::Debug for HandlerError<E, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ExtractorError(e) => write!(f, "{e:?}"),
            Self::ResponderError(e) => write!(f, "{e:?}"),
        }
    }
}

impl<E: Extractor, R: Responder> http_kit::HttpError for HandlerError<E, R> {
    fn status(&self) -> http_kit::StatusCode {
        match self {
            Self::ExtractorError(e) => e.status(),
            Self::ResponderError(e) => e.status(),
        }
    }
}

impl<E: Extractor, R: Responder> core::error::Error for HandlerError<E, R> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::ExtractorError(e) => Some(e),
            Self::ResponderError(e) => Some(e),
        }
    }
}

/// An HTTP handler.
/// This trait is a wrapper trait for `Fn` types. You will rarely use this type directly.
// Verified rendering at the `.at(handler)` registration site:
//   error[E0277]: `fn(NotAnExtractor) -> impl Future<Output = &'static str> {bad_arg}` is not a
//                 Skyzen handler
//     --> src/main.rs:11:33
//      |
//   11 |     let _ = Route::new(("/a".at(bad_arg),));
//      |                              -- ^^^^^^^ not a handler
//      = note: a handler is an `async fn` (or closure) whose arguments all implement `Extractor`
//              and whose return type implements `Responder`
//      = note: a handler takes at most 15 arguments; ...
//      = note: common fixes: ...
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Skyzen handler",
    label = "not a handler",
    note = "a handler is an `async fn` (or closure) whose arguments all implement `Extractor` and whose return type implements `Responder`",
    note = "a handler takes at most 15 arguments; group the rest into one extractor",
    note = "common fixes: take path parameters as `Path<T>` or `Params`, take a body as `Json<T>`/`Bytes`/`String`, and return `Json<T>`, `String` or `Result<T>` rather than a bare value",
    note = "a payload carried by `Json<T>`, `Form<T>`, `Query<T>` or `PrettyJson<T>` must also derive `ToSchema`, since that is what the generated OpenAPI document describes it with: `#[derive(Serialize, skyzen::ToSchema)]`"
)]
///
/// Neither the handler nor the future it returns has to be `Send`: a router is built and served on
/// one thread, so a handler may capture an `Rc` of per-thread state and hold it across an `.await`.
pub trait Handler<T: Extractor, R: Responder>: Clone + 'static {
    /// Handle the request and make a response.
    fn call_handler(
        &self,
        request: &mut Request,
    ) -> impl Future<Output = Result<Response, HandlerError<T, R>>>;
}

/// Adapter that turns a strongly typed [`Handler`] into an [`Endpoint`].
#[derive(Debug)]
pub struct IntoEndpoint<H: Handler<T, R>, T: Extractor, R: Responder> {
    handler: H,
    _marker: PhantomData<(T, R)>,
}

/// Transform handler to endpoint.
pub const fn into_endpoint<T: Extractor, R: Responder, H: Handler<T, R>>(
    handler: H,
) -> IntoEndpoint<H, T, R> {
    IntoEndpoint::new(handler)
}

impl<H: Handler<T, R>, T: Extractor, R: Responder> IntoEndpoint<H, T, R> {
    /// Create an `IntoEndpoint` instance.
    pub const fn new(handler: H) -> Self {
        Self {
            handler,
            _marker: PhantomData,
        }
    }
}

impl<H, T, R> Clone for IntoEndpoint<H, T, R>
where
    H: Handler<T, R> + Clone,
    T: Extractor,
    R: Responder,
{
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
            _marker: PhantomData,
        }
    }
}

macro_rules! impl_handler {
    // A handler taking no arguments extracts nothing. Its extractor is `()`, whose error type is
    // `Infallible`, so the variadic arm's `map_err` would be a call that can never run — which is
    // what nightly reports as an unreachable call. The arm is written out separately rather than
    // silenced, the same way `skyzen_core`'s own `()` extractor is.
    () => {
        impl<F, Fut, Res> Handler<(), Res> for F
        where
            F: 'static + Clone + Fn() -> Fut,
            Fut: Future<Output = Res>,
            Res: Responder,
        {
            async fn call_handler(&self, request: &mut Request) -> Result<Response, HandlerError<(), Res>> {
                let mut response = Response::new(http_kit::Body::empty());
                (self)().await.respond_to(request,&mut response).map_err(|e| HandlerError::ResponderError(e))?;
                Ok(response)
            }
        }
    };
    ($($ty:ident),+) => {
        #[allow(non_snake_case)]

        impl<F, Fut, Res,$($ty:Extractor,)+> Handler<($($ty,)+) , Res> for F
        where
            F: 'static + Clone + Fn($($ty,)+) -> Fut,
            Fut: Future<Output = Res>,
            Res: Responder,
        {
            async fn call_handler(&self, request: &mut Request) -> Result<Response, HandlerError<($($ty,)+), Res>> {
                let ($($ty,)+) = <($($ty,)+) as Extractor>::extract(request).await.map_err(|e| HandlerError::ExtractorError(e))?;
                let mut response = Response::new(http_kit::Body::empty());
                (self)($($ty,)+).await.respond_to(request,&mut response).map_err(|e| HandlerError::ResponderError(e))?;
                Ok(response)
            }
        }
    };
}

tuples!(impl_handler);

impl<H: Handler<T, R>, T: Extractor, R: Responder> Endpoint for IntoEndpoint<H, T, R> {
    type Error = HandlerError<T, R>;
    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        self.handler.call_handler(request).await
    }
}
