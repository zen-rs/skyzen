//! Skyzen's middleware trait and the chain that drives it.
//!
//! A middleware observes a request on its way in, decides whether the rest of the chain runs, and
//! post-processes whatever comes back. Unlike the `http-kit` trait it replaces, [`Middleware`]
//! takes `&self`: the router stores every middleware behind an [`Arc`] and never clones it per
//! request, so a counter, a rate limiter or a connection cache written the obvious way actually
//! keeps its state across requests.
//!
//! ```rust
//! use core::sync::atomic::{AtomicUsize, Ordering};
//! use skyzen_core::{
//!     middleware::{Middleware, Next},
//!     Error, Request, Response,
//! };
//!
//! #[derive(Debug, Default)]
//! struct CountRequests {
//!     seen: AtomicUsize,
//! }
//!
//! impl Middleware for CountRequests {
//!     async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
//!         self.seen.fetch_add(1, Ordering::Relaxed);
//!         next.run(request).await
//!     }
//! }
//! ```
//!
//! Closures work too, through [`from_fn`], and they may capture shared state:
//!
//! ```rust
//! use std::sync::Arc;
//! use skyzen_core::middleware::from_fn;
//!
//! let banner: Arc<str> = Arc::from("skyzen");
//! let middleware = from_fn(move |request, next| {
//!     let banner = Arc::clone(&banner);
//!     Box::pin(async move {
//!         let mut response = next.run(request).await?;
//!         response.headers_mut().insert(
//!             skyzen_core::header::SERVER,
//!             skyzen_core::header::HeaderValue::from_str(&banner).expect("ascii banner"),
//!         );
//!         Ok(response)
//!     })
//! });
//! ```

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    any::{type_name, TypeId},
    fmt::{self, Debug},
    future::Future,
    pin::Pin,
};

use http_kit::{Endpoint, Request, Response};

use crate::error::Error;

/// A boxed future produced by the object-safe halves of the middleware machinery.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Processes a request on its way to the endpoint and the response on its way back.
///
/// Implementors take `&self`, so a middleware value is shared by every request that passes through
/// it. Keep mutable state in an atomic, a lock-free structure or a channel rather than in `&mut
/// self`, which this trait deliberately does not offer.
pub trait Middleware: Send + Sync + 'static {
    /// Handle one request, optionally running the rest of the chain via [`Next::run`].
    ///
    /// # Errors
    ///
    /// Returns whatever the rest of the chain failed with, or the middleware's own rejection.
    fn handle(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> impl Future<Output = Result<Response, Error>> + Send;

    /// Types this middleware inserts into the request, for build-time wiring validation.
    ///
    /// A middleware that injects a value which some extractor later reads back — application
    /// state, an authenticated user — reports that value's [`TypeId`] here so
    /// `Route::try_build` can prove every endpoint's dependencies are wired.
    #[must_use]
    fn provisions(&self) -> Vec<TypeId> {
        Vec::new()
    }
}

/// Object-safe mirror of [`Middleware`], used to store middleware behind a trait object.
///
/// Blanket-implemented for every [`Middleware`]; never implement it directly.
#[doc(hidden)]
pub trait MiddlewareObj: Send + Sync + 'static {
    fn handle_dyn<'a>(
        &'a self,
        request: &'a mut Request,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<Response, Error>>;

    fn provisions_dyn(&self) -> Vec<TypeId>;

    fn middleware_name(&self) -> &'static str;
}

impl<T: Middleware> MiddlewareObj for T {
    fn handle_dyn<'a>(
        &'a self,
        request: &'a mut Request,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(self.handle(request, next))
    }

    fn provisions_dyn(&self) -> Vec<TypeId> {
        self.provisions()
    }

    fn middleware_name(&self) -> &'static str {
        type_name::<T>()
    }
}

/// A middleware stored behind a trait object, ready to be shared across requests.
pub type BoxMiddleware = Arc<dyn MiddlewareObj>;

/// Wrap a middleware for storage in a chain.
#[must_use]
pub fn boxed(middleware: impl Middleware) -> BoxMiddleware {
    Arc::new(middleware)
}

/// The innermost step of a middleware chain: whatever produces the response once every
/// middleware has had its turn.
///
/// Implemented by the router (route matching plus the endpoint) and by
/// endpoint adapters; never implement it directly.
#[doc(hidden)]
pub trait Dispatch: Send + Sync {
    fn dispatch<'a>(&'a self, request: &'a mut Request) -> BoxFuture<'a, Result<Response, Error>>;
}

/// The remainder of a middleware chain.
///
/// Call [`Next::run`] to continue, or drop it to short-circuit with your own response.
pub struct Next<'a> {
    remaining: &'a [BoxMiddleware],
    terminal: &'a dyn Dispatch,
}

impl Debug for Next<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Next")
            .field("remaining", &self.remaining.len())
            .finish_non_exhaustive()
    }
}

impl<'a> Next<'a> {
    /// Build a chain over `remaining`, ending at `terminal`.
    #[doc(hidden)]
    #[must_use]
    pub const fn new(remaining: &'a [BoxMiddleware], terminal: &'a dyn Dispatch) -> Self {
        Self {
            remaining,
            terminal,
        }
    }

    /// How many middleware are still ahead of the endpoint.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.remaining.len()
    }

    /// Run the rest of the chain.
    ///
    /// # Errors
    ///
    /// Propagates the endpoint's error, or any rejection produced by a middleware further in.
    pub async fn run(self, request: &mut Request) -> Result<Response, Error> {
        match self.remaining.split_first() {
            Some((current, remaining)) => {
                let next = Self {
                    remaining,
                    terminal: self.terminal,
                };
                current.handle_dyn(request, next).await
            }
            None => self.terminal.dispatch(request).await,
        }
    }
}

/// Run one middleware around one endpoint, for a single request.
///
/// The chain machinery normally lives inside the router; this is the direct form, for unit tests
/// that exercise a middleware on its own and for embedding a middleware around a bare endpoint.
///
/// # Errors
///
/// Propagates whatever the middleware or the endpoint failed with.
pub async fn apply<M, E>(
    middleware: &M,
    request: &mut Request,
    endpoint: E,
) -> Result<Response, Error>
where
    M: Middleware,
    E: Endpoint + Clone + Send + Sync + 'static,
{
    let terminal = CloneEndpoint(endpoint);
    middleware.handle(request, Next::new(&[], &terminal)).await
}

/// Chain terminal that serves a request from a fresh clone of a shared endpoint.
struct CloneEndpoint<E>(E);

impl<E> Dispatch for CloneEndpoint<E>
where
    E: Endpoint + Clone + Send + Sync + 'static,
{
    fn dispatch<'a>(&'a self, request: &'a mut Request) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let mut endpoint = self.0.clone();
            endpoint.respond(request).await.map_err(Error::from)
        })
    }
}

/// The signature [`from_fn`] accepts: a closure over the request and the rest of the chain.
///
/// The returned future borrows the request, so it has to be boxed — a closure has exactly one
/// return type and cannot name a different anonymous future per request lifetime. Write the body
/// as `Box::pin(async move { .. })`; to reuse an existing `async fn`, wrap the call the same way.
pub trait MiddlewareFn:
    for<'a> Fn(&'a mut Request, Next<'a>) -> BoxFuture<'a, Result<Response, Error>>
    + Send
    + Sync
    + 'static
{
}

impl<F> MiddlewareFn for F where
    F: for<'a> Fn(&'a mut Request, Next<'a>) -> BoxFuture<'a, Result<Response, Error>>
        + Send
        + Sync
        + 'static
{
}

/// Build a [`Middleware`] out of an async closure.
///
/// The closure receives the request and the rest of the chain, exactly like
/// [`Middleware::handle`]. Because the value is shared rather than cloned per request, a closure
/// capturing an `Arc` of shared state behaves the way it reads.
///
/// ```rust
/// use skyzen_core::middleware::{from_fn, Next};
///
/// let middleware = from_fn(|request, next: Next<'_>| {
///     Box::pin(async move { next.run(request).await })
/// });
/// ```
///
/// Prefer implementing [`Middleware`] directly when the middleware has a name worth having: the
/// trait's `async fn handle` needs no boxing.
pub const fn from_fn<F: MiddlewareFn>(f: F) -> FromFn<F> {
    FromFn(f)
}

/// A [`Middleware`] built from a closure by [`from_fn`].
pub struct FromFn<F>(F);

impl<F> Debug for FromFn<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FromFn").field(&type_name::<F>()).finish()
    }
}

impl<F: MiddlewareFn> Middleware for FromFn<F> {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        (self.0)(request, next).await
    }
}

#[cfg(test)]
mod tests {
    use super::{boxed, from_fn, BoxFuture, BoxMiddleware, Dispatch, Middleware, Next};
    use crate::error::Error;
    use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use futures_lite::future::block_on;
    use http_kit::{header::HeaderValue, Body, Request, Response};

    struct Echo;

    impl Dispatch for Echo {
        fn dispatch<'a>(
            &'a self,
            _request: &'a mut Request,
        ) -> BoxFuture<'a, Result<Response, Error>> {
            Box::pin(async { Ok(Response::new(Body::from("ok"))) })
        }
    }

    /// Shared scratch space every layer in a test chain writes to.
    #[derive(Debug, Default)]
    struct Trace {
        clock: AtomicUsize,
        outer_at: AtomicUsize,
        inner_at: AtomicUsize,
        inner_calls: AtomicUsize,
    }

    impl Trace {
        fn tick(&self) -> usize {
            self.clock.fetch_add(1, Ordering::SeqCst)
        }
    }

    /// A middleware holding state directly — the exact shape that silently no-opped before.
    struct RecordInner(Arc<Trace>);

    impl Middleware for RecordInner {
        async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
            self.0.inner_calls.fetch_add(1, Ordering::SeqCst);
            self.0.inner_at.store(self.0.tick(), Ordering::SeqCst);
            next.run(request).await
        }
    }

    /// An `async fn` item reused as a middleware body through `from_fn`.
    async fn passthrough(request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        next.run(request).await
    }

    fn drive(layers: &[BoxMiddleware]) -> Response {
        let mut request = Request::new(Body::empty());
        block_on(Next::new(layers, &Echo).run(&mut request)).expect("chain succeeds")
    }

    #[test]
    fn two_layers_run_outermost_first_and_share_state_across_requests() {
        let trace = Arc::new(Trace::default());

        let outer = {
            let trace = Arc::clone(&trace);
            from_fn(move |request, next: Next<'_>| {
                let trace = Arc::clone(&trace);
                Box::pin(async move {
                    trace.outer_at.store(trace.tick(), Ordering::SeqCst);
                    next.run(request).await
                })
            })
        };

        let layers: Vec<BoxMiddleware> = vec![
            boxed(outer),
            boxed(from_fn(|request, next| {
                Box::pin(passthrough(request, next))
            })),
            boxed(RecordInner(Arc::clone(&trace))),
        ];

        drive(&layers);
        drive(&layers);

        // The middleware values are shared, not cloned per request, so the count survives.
        assert_eq!(trace.inner_calls.load(Ordering::SeqCst), 2);
        assert!(trace.outer_at.load(Ordering::SeqCst) < trace.inner_at.load(Ordering::SeqCst));
    }

    #[test]
    fn a_layer_can_short_circuit_without_running_the_rest_of_the_chain() {
        let trace = Arc::new(Trace::default());
        let layers: Vec<BoxMiddleware> = vec![
            boxed(from_fn(|_request, _next| {
                Box::pin(async {
                    let mut response = Response::new(Body::empty());
                    response
                        .headers_mut()
                        .insert("x-short-circuit", HeaderValue::from_static("1"));
                    Ok(response)
                })
            })),
            boxed(RecordInner(Arc::clone(&trace))),
        ];

        let response = drive(&layers);

        assert!(response.headers().contains_key("x-short-circuit"));
        assert_eq!(trace.inner_calls.load(Ordering::SeqCst), 0);
    }
}
