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

impl<E: Extractor, R: Responder> core::error::Error for HandlerError<E, R> {}

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
//      = note: the future a handler returns must be `Send`: ...
//      = note: a handler takes at most 15 arguments; ...
//      = note: common fixes: ...
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Skyzen handler",
    label = "not a handler",
    note = "a handler is an `async fn` (or closure) whose arguments all implement `Extractor` and whose return type implements `Responder`",
    note = "the future a handler returns must be `Send`: do not hold an `Rc` or a `MutexGuard` across an `.await`",
    note = "a handler takes at most 15 arguments; group the rest into one extractor",
    note = "common fixes: take path parameters as `Path<T>` or `Params`, take a body as `Json<T>`/`Bytes`/`String`, and return `Json<T>`, `String` or `Result<T>` rather than a bare value"
)]
pub trait Handler<T: Extractor, R: Responder>: Send + Sync + Clone + 'static {
    /// Handle the request and make a response.
    fn call_handler(
        &self,
        request: &mut Request,
    ) -> impl Future<Output = Result<Response, HandlerError<T, R>>> + Send;
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
    ($($ty:ident),*) => {
        #[allow(non_snake_case)]

        impl<F, Fut, Res,$($ty:Extractor,)*> Handler<($($ty,)*) , Res> for F
        where
            F: 'static + Clone + Send + Sync + Fn($($ty,)*) -> Fut,
            Fut: Send + Future<Output = Res>,
            Res: Responder,
        {
            async fn call_handler(&self, request: &mut Request) -> Result<Response, HandlerError<($($ty,)*), Res>> {
                let ($($ty,)*) = <($($ty,)*) as Extractor>::extract(request).await.map_err(|e| HandlerError::ExtractorError(e))?;
                let mut response = Response::new(http_kit::Body::empty());
                (self)($($ty,)*).await.respond_to(request,&mut response).map_err(|e| HandlerError::ResponderError(e))?;
                Ok(response)
            }
        }
    };
}

tuples!(impl_handler);

impl<H: Handler<T, R> + Send + Sync, T: Extractor + Send + Sync, R: Responder + Send + Sync>
    Endpoint for IntoEndpoint<H, T, R>
{
    type Error = HandlerError<T, R>;
    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        self.handler.call_handler(request).await
    }
}
