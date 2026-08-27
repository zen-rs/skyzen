//! Tree-based routing primitives.
//!
//! Routes are defined by combining nodes produced by the [`CreateRouteNode`] extension. Path
//! literals gain builder methods such as `.at(handler)` (GET), `.post(handler)`, `.put(handler)`,
//! `.patch(handler)`, `.delete(handler)`, `.head(handler)`, `.options(handler)`,
//! `.trace(handler)`, `.on(method, handler)`, `.any(handler)`, `.ws(handler)`, `.route(children)`
//! and `.nest(router)` so you can describe the full tree declaratively. Once a tree is assembled,
//! call [`Route::build`] to obtain a [`Router`] that can be mounted on a server or invoked
//! directly from tests.
//!
//! ## Building routes
//! ```no_run
//! use skyzen::{
//!     routing::{CreateRouteNode, Params, Route},
//!     Result,
//! };
//!
//! async fn ping() -> Result<&'static str> {
//!     Ok("pong")
//! }
//!
//! async fn hello(params: Params) -> Result<String> {
//!     let name = params.get("name")?;
//!     Ok(format!("Hello, {name}!"))
//! }
//!
//! let router = Route::new((
//!     "/ping".at(ping),
//!     "/user".route((
//!         "/{name}".at(hello),
//!     )),
//! ))
//! .build();
//! ```
//!
//! A tuple holds at most 15 nodes. Larger services compose with `vec![...]` (any
//! [`IntoRouteNode`] element) or by nesting whole [`Route`] values.
//!
//! ## Named parameters and wildcards
//! Use `{name}` to capture a single path segment and `{*path}` to capture the rest of the path.
//! [`Path<T>`](crate::extract::Path) deserializes the captures into a struct, a tuple or a single
//! primitive; [`Params`] reads them by name when the names are only known at runtime:
//! ```no_run
//! use skyzen::{
//!     routing::{CreateRouteNode, Params, Route},
//!     Result,
//! };
//!
//! async fn echo(params: Params) -> Result<String> {
//!     let path = params.get("path")?;
//!     Ok(format!("Path: {path}"))
//! }
//!
//! let route = Route::new(("/files/{*path}".at(echo),));
//! ```
//!
//! ## Applying middleware
//! Middleware attaches at three scopes, from narrowest to widest:
//!
//! - [`RouteNode::with`] wraps a single node's endpoints,
//! - [`Route::with`] wraps every endpoint reachable from a route,
//! - [`Route::layer`] wraps the whole router — every endpoint *and* the 404 and 405 responses,
//!   which is the scope CORS, tracing and request-id layers need.
//!
//! ```no_run
//! use skyzen::{
//!     routing::{CreateRouteNode, Route},
//!     utils::State,
//!     Result,
//! };
//!
//! let route = Route::new(("/counter".at(|| async { Result::Ok("0") }),))
//! .with(State(0usize));
//! ```
//!
//! Error handling can also be expressed as middleware. For example, you can catch endpoint errors
//! with [`ErrorHandlingMiddleware`](crate::middleware::ErrorHandlingMiddleware):
//! ```no_run
//! use skyzen::{
//!     middleware::ErrorHandlingMiddleware,
//!     routing::{CreateRouteNode, Route},
//!     Result,
//! };
//!
//! async fn boom() -> Result<&'static str> {
//!     Err(skyzen::Error::msg("boom"))
//! }
//!
//! let router = Route::new(("/panic".at(boom),))
//! .with(ErrorHandlingMiddleware::new(|error| async move {
//!     format!("Recovered from {error}")
//! }))
//! .build();
//! ```
//!
//! ## Mounting a built router
//! [`CreateRouteNode::nest`] hangs an already-built [`Router`] — one a library exported, say —
//! under a path prefix. The mounted router matches as if it sat at the root and keeps its own
//! fallback and `405`.
//!
//! ## Replacing the 404 and 405 responses
//! [`Route::fallback`] and [`Route::method_not_allowed`] take ordinary handlers. The
//! method-not-allowed handler can read the registered methods back with [`AllowedMethods`].
//!
//! ## WebSocket routes
//! When the `ws` feature is enabled you can use `.ws` to accept upgrades without manually
//! extracting [`WebSocketUpgrade`](crate::websocket::WebSocketUpgrade):
//! ```no_run
//! use futures_util::StreamExt;
//! use skyzen::routing::{CreateRouteNode, Route};
//!
//! let routes = Route::new((
//!     "/chat".ws(|mut socket| async move {
//!         while let Some(message) = socket.next().await {
//!             if let Some(text) = message?.into_text() {
//!                 socket.send_text(text).await?;
//!             }
//!         }
//!         Ok::<_, skyzen::Error>(())
//!     }),
//! ));
//! ```
//! The `.ws` builder enforces the HTTP upgrade requirements automatically. A session that returns
//! an error is logged and closed with
//! [`websocket::INTERNAL_ERROR`](crate::websocket::INTERNAL_ERROR); a session that returns `()`
//! keeps the older, quieter behaviour. The same route compiles on `wasm32`, where session futures
//! hold JS handles and are never `Send`.
//!
//! Middleware is applied from the outermost route to the innermost endpoint, so errors bubble up
//! until they are handled.

use std::{fmt, sync::Arc};

#[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
use crate::openapi::RouteOpenApiEntry;
#[cfg(feature = "ws")]
use crate::websocket::{MaybeSend, MaybeSync, WebSocket};
use crate::{handler, handler::Handler, middleware::Middleware, openapi, openapi::OpenApi};
use http_kit::endpoint::AnyEndpoint;
use http_kit::{Endpoint, Method};
use skyzen_core::{
    middleware::boxed, middleware::BoxMiddleware, Extractor, Requirement, Responder,
};
#[cfg(feature = "ws")]
use std::future::Future;

/// Type alias for dynamically dispatched endpoints stored in the routing tree.
pub type BoxEndpoint = AnyEndpoint;
pub(crate) type EndpointFactory = Arc<dyn Fn() -> BoxEndpoint + Send + Sync>;

// Export param types
mod param;
pub use param::{MissingParam, Params};

// Export router types
mod router;
pub use router::{build, AllowedMethods, NotFound, RouteBuildError, Router};

mod nest;
pub use nest::{NestedPathError, NestedRouter};

/// Which requests an endpoint node answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodFilter {
    /// Only requests using this exact method.
    Exact(Method),
    /// Every method that reaches the path, whatever it is.
    Any,
}

impl fmt::Display for MethodFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(method) => f.write_str(method.as_str()),
            Self::Any => f.write_str("ANY"),
        }
    }
}

/// A handler registered on a route, together with the wiring it depends on.
struct EndpointEntry {
    factory: EndpointFactory,
    requirements: Vec<Requirement>,
}

impl fmt::Debug for EndpointEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointEntry")
            .field("requirements", &self.requirements.len())
            .finish_non_exhaustive()
    }
}

/// Collection of route nodes anchored at a path prefix.
pub struct Route {
    /// All nodes that hang off the route's mount point.
    nodes: Vec<RouteNode>,
    /// Middleware wrapping the whole router, outermost first.
    layers: Vec<BoxMiddleware>,
    /// Replacement for the built-in 404 response.
    fallback: Option<EndpointEntry>,
    /// Replacement for the built-in 405 response.
    method_not_allowed: Option<EndpointEntry>,
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route")
            .field("nodes", &self.nodes)
            .field("layers", &self.layers.len())
            .field("has_fallback", &self.fallback.is_some())
            .field("has_method_not_allowed", &self.method_not_allowed.is_some())
            .finish()
    }
}

/// A single node in the routing tree.
#[derive(Debug)]
pub struct RouteNode {
    /// The literal path segment represented by this node.
    path: String,
    /// The kind of node.
    node_type: RouteNodeType,
}

/// Distinguishes between nested routes and terminal endpoints.
pub enum RouteNodeType {
    /// Sub-route with additional child nodes.
    Route(Route),
    /// Terminal endpoint located at the provided path and method.
    Endpoint {
        /// Factory producing a fresh endpoint that can be safely shared.
        endpoint_factory: EndpointFactory,
        /// Which requests the node answers.
        method: MethodFilter,
        /// Handler metadata for `OpenAPI` export.
        openapi: Option<openapi::RouteHandlerDoc>,
        /// Values the handler's extractors need middleware to provide.
        requirements: Vec<Requirement>,
        /// Middleware wrapping this endpoint, outermost first.
        middleware: Vec<BoxMiddleware>,
    },
    /// An already-built [`Router`] mounted under the node's path.
    Nested {
        /// The mounted router, which sees paths with the mount prefix removed.
        router: Router,
        /// Middleware wrapping the mounted router, outermost first.
        middleware: Vec<BoxMiddleware>,
    },
}

impl fmt::Debug for RouteNodeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Route(route) => f.debug_tuple("Route").field(route).finish(),
            Self::Endpoint {
                method, middleware, ..
            } => f
                .debug_struct("Endpoint")
                .field("method", method)
                .field("middleware", &middleware.len())
                .finish(),
            Self::Nested { router, middleware } => f
                .debug_struct("Nested")
                .field("router", router)
                .field("middleware", &middleware.len())
                .finish(),
        }
    }
}

impl Route {
    /// Build a [`Route`] from the provided nodes.
    #[must_use]
    pub fn new(nodes: impl Routes) -> Self {
        Self {
            nodes: nodes.into_route_nodes(),
            layers: Vec::new(),
            fallback: None,
            method_not_allowed: None,
        }
    }

    /// Register an alarm handler for Durable Object alarm events.
    ///
    /// The handler is an async function with extractors as arguments,
    /// just like a regular route handler.
    #[must_use]
    pub fn on_alarm<H, T, R>(self, handler: H) -> RouteWithAlarm
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        let endpoint = handler::into_endpoint(handler);
        RouteWithAlarm {
            route: self,
            alarm_endpoint: Arc::new(move || AnyEndpoint::new(endpoint.clone())),
        }
    }

    /// Attach middleware to this route and all nested endpoints.
    #[must_use]
    pub fn middleware<M: Middleware>(mut self, middleware: M) -> Self {
        self.apply_middleware(&boxed(middleware));
        self
    }

    /// Attach middleware to this route and all nested endpoints.
    ///
    /// This is an ergonomic alias for [`Route::middleware`].
    #[must_use]
    pub fn with<M: Middleware>(self, middleware: M) -> Self {
        self.middleware(middleware)
    }

    /// Wrap the entire router in `middleware`.
    ///
    /// Unlike [`Route::with`], a layer also runs for requests that match no route and for
    /// requests whose method is not registered — so a CORS layer sees the preflight `OPTIONS`
    /// that would otherwise be answered by the built-in 405, and a tracing layer sees 404s.
    /// Layers run outermost-first in registration order.
    #[must_use]
    pub fn layer<M: Middleware>(mut self, middleware: M) -> Self {
        self.layers.push(boxed(middleware));
        self
    }

    /// Answer requests that match no route with `handler` instead of the built-in 404.
    #[must_use]
    pub fn fallback<H, T, R>(mut self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.fallback = Some(endpoint_entry(handler));
        self
    }

    /// Answer requests whose method is not registered for a matched path with `handler`
    /// instead of the built-in 405.
    ///
    /// The handler can read the registered methods with the [`AllowedMethods`] extractor; the
    /// built-in response renders them into the `Allow` header.
    #[must_use]
    pub fn method_not_allowed<H, T, R>(mut self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.method_not_allowed = Some(endpoint_entry(handler));
        self
    }

    fn apply_middleware(&mut self, middleware: &BoxMiddleware) {
        for node in &mut self.nodes {
            node.apply_middleware(middleware);
        }
    }

    /// Convert this route into plain nodes for mounting inside a larger tree.
    ///
    /// Router-wide settings have no meaning below the root, so layers are demoted to subtree
    /// middleware and a fallback is rejected outright rather than silently dropped.
    ///
    /// # Panics
    ///
    /// Panics if a fallback or method-not-allowed handler was registered on a nested route.
    fn into_mounted_nodes(mut self) -> Vec<RouteNode> {
        assert!(
            self.fallback.is_none() && self.method_not_allowed.is_none(),
            "`fallback` and `method_not_allowed` belong to the router as a whole; register them \
             on the outermost `Route` rather than on one that is mounted inside another"
        );
        // Applying pushes to the front of each endpoint's stack, so replaying the layers in
        // reverse leaves the first-registered layer outermost, exactly as at the root.
        let layers = std::mem::take(&mut self.layers);
        for layer in layers.iter().rev() {
            self.apply_middleware(layer);
        }
        self.nodes
    }

    /// Build the route, panicking on error.
    ///
    /// # Panics
    /// Panics if the route is invalid: see [`RouteBuildError`] for what is rejected. Use
    /// [`Route::try_build`] to handle the failure yourself.
    #[must_use]
    pub fn build(self) -> Router {
        self.try_build().unwrap_or_else(|error| panic!("{error}"))
    }

    /// Build the route, reporting an invalid tree instead of panicking.
    ///
    /// # Errors
    ///
    /// Returns [`RouteBuildError`] when a path is unusable, a method is registered twice for one
    /// path, or an endpoint needs a value no middleware on its ancestor chain provides.
    pub fn try_build(self) -> Result<Router, RouteBuildError> {
        build(self)
    }

    /// Generate an [`OpenApi`] document describing this route tree.
    #[must_use]
    pub fn openapi(&self) -> OpenApi {
        #[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
        {
            let mut entries = Vec::new();
            collect_openapi_entries("", &self.nodes, &mut entries);
            OpenApi::from_entries(&entries)
        }

        #[cfg(not(all(feature = "openapi", not(target_arch = "wasm32"))))]
        {
            OpenApi::default()
        }
    }

    /// Enable the Redoc API documentation endpoint at `/api-docs`.
    #[must_use]
    pub fn enable_api_doc(mut self) -> Self {
        let openapi = self.openapi();
        self.nodes
            .push(openapi.redoc_route(openapi::DEFAULT_API_DOCS_MOUNT));
        self
    }
}

/// A [`Route`] with an attached alarm handler.
///
/// Created by [`Route::on_alarm`]. Call [`build`](Self::build) to produce a [`Router`].
pub struct RouteWithAlarm {
    route: Route,
    alarm_endpoint: EndpointFactory,
}

#[allow(clippy::missing_fields_in_debug)]
impl fmt::Debug for RouteWithAlarm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteWithAlarm")
            .field("route", &self.route)
            .field("has_alarm", &true)
            .finish()
    }
}

impl RouteWithAlarm {
    /// Attach middleware to this route and all nested endpoints.
    #[must_use]
    pub fn middleware<M: Middleware>(mut self, middleware: M) -> Self {
        self.route = self.route.middleware(middleware);
        self
    }

    /// Attach middleware to this route and all nested endpoints.
    ///
    /// This is an ergonomic alias for [`RouteWithAlarm::middleware`].
    #[must_use]
    pub fn with<M: Middleware>(self, middleware: M) -> Self {
        self.middleware(middleware)
    }

    /// Wrap the entire router in `middleware`; see [`Route::layer`].
    #[must_use]
    pub fn layer<M: Middleware>(mut self, middleware: M) -> Self {
        self.route = self.route.layer(middleware);
        self
    }

    /// Build the route, panicking on error.
    ///
    /// # Panics
    /// Panics if the route is invalid.
    #[must_use]
    pub fn build(self) -> Router {
        let alarm_factory = self.alarm_endpoint;
        let mut router = self.route.build();
        router.alarm_handler = Some(alarm_factory);
        router
    }

    /// Enable the Redoc API documentation endpoint at `/api-docs`.
    #[must_use]
    pub fn enable_api_doc(mut self) -> Self {
        let openapi = self.route.openapi();
        self.route
            .nodes
            .push(openapi.redoc_route(openapi::DEFAULT_API_DOCS_MOUNT));
        self
    }
}

impl RouteNode {
    /// Construct an endpoint node with the provided handler.
    #[must_use]
    pub(crate) fn new_endpoint<E>(
        path: impl Into<String>,
        method: MethodFilter,
        endpoint: E,
        openapi: Option<openapi::RouteHandlerDoc>,
        requirements: Vec<Requirement>,
    ) -> Self
    where
        E: Endpoint + Clone + Send + Sync + 'static,
    {
        let endpoint_factory: EndpointFactory =
            Arc::new(move || AnyEndpoint::new(endpoint.clone()));
        Self {
            path: path.into(),
            node_type: RouteNodeType::Endpoint {
                endpoint_factory,
                method,
                openapi,
                requirements,
                middleware: Vec::new(),
            },
        }
    }

    /// Construct a nested route node mounted under `path`.
    #[must_use]
    pub(crate) fn new_route(path: impl Into<String>, route: Route) -> Self {
        Self {
            path: path.into(),
            node_type: RouteNodeType::Route(route),
        }
    }

    /// Construct a node mounting an already-built router under `path`.
    #[must_use]
    pub(crate) fn new_nested(path: impl Into<String>, router: Router) -> Self {
        Self {
            path: path.into(),
            node_type: RouteNodeType::Nested {
                router,
                middleware: Vec::new(),
            },
        }
    }

    fn apply_middleware(&mut self, middleware: &BoxMiddleware) {
        match &mut self.node_type {
            RouteNodeType::Route(route) => route.apply_middleware(middleware),
            RouteNodeType::Endpoint {
                middleware: stack, ..
            }
            | RouteNodeType::Nested {
                middleware: stack, ..
            } => stack.insert(0, Arc::clone(middleware)),
        }
    }

    /// Attach middleware to just this node's endpoints.
    ///
    /// ```no_run
    /// use skyzen::{middleware::CompressionMiddleware, routing::{CreateRouteNode, Route}, Result};
    ///
    /// let route = Route::new((
    ///     "/report".at(|| async { Result::Ok("...") }).with(CompressionMiddleware::new()),
    /// ));
    /// ```
    #[must_use]
    pub fn with<M: Middleware>(mut self, middleware: M) -> Self {
        self.apply_middleware(&boxed(middleware));
        self
    }
}

impl RouteNode {
    /// Attach a GET handler to the current route node.
    #[must_use]
    pub fn at<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::GET, handler)
    }

    /// Alias for [`RouteNode::at`].
    #[must_use]
    pub fn get<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.at(handler)
    }

    /// Attach a POST handler to the current route node.
    #[must_use]
    pub fn post<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::POST, handler)
    }

    /// Attach a PATCH handler to the current route node.
    #[must_use]
    pub fn patch<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::PATCH, handler)
    }

    /// Attach a PUT handler to the current route node.
    #[must_use]
    pub fn put<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::PUT, handler)
    }

    /// Attach a DELETE handler to the current route node.
    #[must_use]
    pub fn delete<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::DELETE, handler)
    }

    /// Attach a HEAD handler to the current route node.
    #[must_use]
    pub fn head<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::HEAD, handler)
    }

    /// Attach an OPTIONS handler to the current route node.
    #[must_use]
    pub fn options<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::OPTIONS, handler)
    }

    /// Attach a TRACE handler to the current route node.
    #[must_use]
    pub fn trace<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::TRACE, handler)
    }

    /// Attach a handler for an arbitrary HTTP method, keeping its `OpenAPI` metadata.
    #[must_use]
    pub fn on<H, T, R>(self, method: Method, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.with_handler(MethodFilter::Exact(method), handler)
    }

    /// Attach a handler that answers every method reaching this path.
    ///
    /// A path with an `any` handler never produces a 405; more specific method registrations on
    /// the same path still win. Because `any` covers `HEAD` itself, such a request is answered by
    /// the handler rather than by the GET fallback, so the handler is responsible for its own
    /// empty body. `any` carries no `OpenAPI` operation, since there is no single method to
    /// document it under.
    #[must_use]
    pub fn any<H, T, R>(self, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.with_handler(MethodFilter::Any, handler)
    }

    /// Attach an endpoint under the current path with an arbitrary HTTP method.
    #[must_use]
    pub fn endpoint<E>(self, method: Method, endpoint: E) -> Self
    where
        E: Endpoint + Clone + Send + Sync + 'static,
    {
        self.extend_with_nodes(vec![Self::new_endpoint(
            "",
            MethodFilter::Exact(method),
            endpoint,
            None,
            Vec::new(),
        )])
    }

    /// Attach additional child routes under the current path.
    #[must_use]
    pub fn route(self, routes: impl Routes) -> Self {
        self.extend_with_nodes(routes.into_route_nodes())
    }

    /// Attach a WebSocket handler that performs the upgrade under the current path.
    ///
    /// See [`CreateRouteNode::ws`] for what a session may return and what the framework does with
    /// it.
    #[cfg(feature = "ws")]
    #[must_use]
    pub fn ws<F, Fut>(self, session: F) -> Self
    where
        F: Fn(WebSocket) -> Fut + Clone + MaybeSend + MaybeSync + 'static,
        Fut: Future + MaybeSend + 'static,
        Fut::Output: crate::websocket::IntoWebSocketOutcome + 'static,
    {
        self.at(crate::websocket::session_handler(session))
    }

    fn with_handler<H, T, R>(self, method: MethodFilter, handler: H) -> Self
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        let endpoint = endpoint_node_from_handler("", method, handler);
        self.extend_with_nodes(vec![endpoint])
    }

    /// Mount an already-built [`Router`] under the current path.
    ///
    /// See [`CreateRouteNode::nest`] for what the mounted router sees.
    #[must_use]
    pub fn nest(self, router: Router) -> Self {
        self.extend_with_nodes(vec![Self::new_nested("", router)])
    }

    fn extend_with_nodes(self, mut additional: Vec<Self>) -> Self {
        let path = self.path;
        let mut nodes = match self.node_type {
            RouteNodeType::Route(route) => route.into_mounted_nodes(),
            terminal @ (RouteNodeType::Endpoint { .. } | RouteNodeType::Nested { .. }) => {
                vec![Self {
                    path: String::new(),
                    node_type: terminal,
                }]
            }
        };

        nodes.append(&mut additional);

        Self {
            path,
            node_type: RouteNodeType::Route(Route::new(nodes)),
        }
    }
}

// Trait for building routes
/// Trait implemented by types that can be converted into route nodes.
// Verified rendering at `Route::new`:
//   error[E0277]: `&str` is not a route tree
//     --> src/main.rs:10:24
//      |
//   10 |     let _ = Route::new("/c");
//      |             ---------- ^^^^ not `Routes`
//      = note: pass a tuple of route nodes — note the trailing comma for a single node:
//              `Route::new(("/ping".at(ping),))`
//      = note: a tuple holds at most 15 nodes; ...
//      = note: a built `Router` is mounted with `"/prefix".nest(router)` rather than passed here
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a route tree",
    label = "not `Routes`",
    note = "pass a tuple of route nodes — note the trailing comma for a single node: `Route::new((\"/ping\".at(ping),))`",
    note = "a tuple holds at most 15 nodes; beyond that use `vec![..]` of route nodes, or group them into nested `Route`s",
    note = "a built `Router` is mounted with `\"/prefix\".nest(router)` rather than passed here"
)]
pub trait Routes {
    /// Consume the type and produce the corresponding route nodes.
    fn into_route_nodes(self) -> Vec<RouteNode>;
}

/// Trait implemented by types that can be converted into a [`RouteNode`].
pub trait IntoRouteNode {
    /// Consume the type and produce the [`RouteNode`].
    fn into_route_node(self) -> RouteNode;
}

impl IntoRouteNode for RouteNode {
    fn into_route_node(self) -> RouteNode {
        self
    }
}

impl IntoRouteNode for Route {
    fn into_route_node(self) -> RouteNode {
        RouteNode::new_route("", Self::new(self.into_mounted_nodes()))
    }
}

impl<T> Routes for Vec<T>
where
    T: IntoRouteNode,
{
    fn into_route_nodes(self) -> Vec<RouteNode> {
        self.into_iter()
            .map(IntoRouteNode::into_route_node)
            .collect()
    }
}

impl Routes for RouteNode {
    fn into_route_nodes(self) -> Vec<RouteNode> {
        vec![self]
    }
}

impl Routes for Route {
    fn into_route_nodes(self) -> Vec<RouteNode> {
        self.into_mounted_nodes()
    }
}

impl Routes for () {
    fn into_route_nodes(self) -> Vec<RouteNode> {
        Vec::new()
    }
}

macro_rules! impl_routes_tuple {
    () => {};
    ($($ty:ident),+) => {
        #[allow(non_snake_case)]
        impl<$($ty,)+> Routes for ($($ty,)+)
        where
            $($ty: IntoRouteNode,)+
        {
            fn into_route_nodes(self) -> Vec<RouteNode> {
                let ($($ty,)+) = self;
                vec![$($ty.into_route_node(),)+]
            }
        }
    };
}

tuples!(impl_routes_tuple);

fn endpoint_entry<H, T, R>(handler: H) -> EndpointEntry
where
    H: Handler<T, R>,
    T: Extractor,
    R: Responder,
{
    let endpoint = handler::into_endpoint(handler);
    EndpointEntry {
        factory: Arc::new(move || AnyEndpoint::new(endpoint.clone())),
        requirements: T::requirements(),
    }
}

fn endpoint_node_from_handler<P, H, T, R>(path: P, method: MethodFilter, handler: H) -> RouteNode
where
    P: Into<String>,
    H: Handler<T, R>,
    T: Extractor,
    R: Responder,
{
    let handler_doc = openapi::describe_handler::<H>();
    let endpoint = handler::into_endpoint(handler);
    RouteNode::new_endpoint(
        path.into(),
        method,
        endpoint,
        Some(handler_doc),
        T::requirements(),
    )
}

/// Join a mount prefix and a node path, collapsing the duplicate separator a nested
/// `"/api/"` + `"/v1"` would otherwise produce.
///
/// A trailing slash is preserved: `/dir` and `/dir/` are distinct routes.
pub(crate) fn join_path(prefix: &str, segment: &str) -> String {
    let mut joined = String::with_capacity(prefix.len() + segment.len());
    joined.push_str(prefix);
    joined.push_str(segment);
    if !joined.contains("//") {
        return joined;
    }

    let mut collapsed = String::with_capacity(joined.len());
    let mut previous_slash = false;
    for character in joined.chars() {
        let is_slash = character == '/';
        if is_slash && previous_slash {
            continue;
        }
        previous_slash = is_slash;
        collapsed.push(character);
    }
    collapsed
}

#[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
fn collect_openapi_entries(
    path_prefix: &str,
    nodes: &[RouteNode],
    buf: &mut Vec<RouteOpenApiEntry>,
) {
    for node in nodes {
        let path = join_path(path_prefix, &node.path);
        match &node.node_type {
            RouteNodeType::Route(route) => {
                collect_openapi_entries(&path, &route.nodes, buf);
            }
            RouteNodeType::Endpoint {
                method, openapi, ..
            } => {
                // `Any` has no single OpenAPI operation to attach the documentation to.
                if let (Some(openapi), MethodFilter::Exact(method)) = (openapi, method) {
                    buf.push(RouteOpenApiEntry::new(path, method.clone(), *openapi));
                }
            }
            RouteNodeType::Nested { router, .. } => {
                buf.extend(prefixed_openapi_entries(&path, router));
            }
        }
    }
}

/// A mounted router's operations, re-pathed under the prefix it is mounted at.
#[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
pub(crate) fn prefixed_openapi_entries(prefix: &str, router: &Router) -> Vec<RouteOpenApiEntry> {
    router
        .openapi_entries()
        .iter()
        .map(|entry| {
            RouteOpenApiEntry::new(
                join_path(prefix, &entry.path),
                entry.method.clone(),
                entry.handler,
            )
        })
        .collect()
}

/// Builder extension that turns a path literal into convenient routing nodes.
pub trait CreateRouteNode: Sized {
    /// Attach a GET handler to the path.
    fn at<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Alias for [`CreateRouteNode::at`].
    fn get<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.at(handler)
    }

    /// Attach a POST handler to the path.
    fn post<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Attach a PATCH handler to the path.
    fn patch<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Attach a PUT handler to the path.
    fn put<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Attach a DELETE handler to the path.
    fn delete<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Attach a HEAD handler to the path.
    fn head<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Attach an OPTIONS handler to the path.
    fn options<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Attach a TRACE handler to the path.
    fn trace<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Attach a handler for an arbitrary HTTP method, keeping its `OpenAPI` metadata.
    fn on<H, T, R>(self, method: Method, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Attach a handler that answers every method reaching this path.
    ///
    /// See [`RouteNode::any`] for how it interacts with `HEAD` and `OpenAPI`.
    fn any<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder;

    /// Mount nested routes under the current path segment.
    fn route(self, routes: impl Routes) -> RouteNode;

    /// Mount an already-built [`Router`] under the current path segment.
    ///
    /// The mounted router matches as if it sat at the root: the prefix is stripped before it sees
    /// the request, and put back afterwards. It keeps its own `404` fallback and `405`, so a path
    /// under the prefix that it does not know is answered by *it* rather than by the outer router.
    /// Its `OpenAPI` operations are re-exported under the prefix.
    ///
    /// ```rust
    /// use skyzen::{routing::{CreateRouteNode, Route}, Result};
    ///
    /// // A library exports a built router...
    /// let admin = Route::new(("/users".at(|| async { Result::Ok("users") }),)).build();
    ///
    /// // ...and an application hangs it wherever it likes.
    /// let router = Route::new(("/admin".nest(admin),)).build();
    /// ```
    ///
    /// Use [`route`](Self::route) instead to compose an unbuilt [`Route`]: nesting exists for the
    /// case where the router is already built and its internals are no longer reachable.
    fn nest(self, router: Router) -> RouteNode;

    /// Attach an endpoint at the specified method and path.
    ///
    /// Note: This is a low-level method; prefer using `.at`, `.post`, etc. for common HTTP methods.
    /// Especially when using `OpenAPI`, those methods will automatically generate documentation.
    fn endpoint<E>(self, method: Method, endpoint: E) -> RouteNode
    where
        E: Endpoint + Clone + Send + Sync + 'static;

    /// Attach a WebSocket handler that automatically performs the upgrade handshake.
    ///
    /// The session owns the socket until it returns. Returning `()` says only that it ended;
    /// returning `Result<(), E>` — for any `E` that converts into [`Error`](crate::Error),
    /// [`WebSocketError`](crate::websocket::WebSocketError) included — lets the session use `?`
    /// and hand back whatever stopped it. The framework logs that error with its whole `source()`
    /// chain and closes the connection with
    /// [`websocket::INTERNAL_ERROR`](crate::websocket::INTERNAL_ERROR).
    ///
    /// ```no_run
    /// use futures_util::StreamExt;
    /// use skyzen::routing::{CreateRouteNode, Route};
    ///
    /// let routes = Route::new((
    ///     "/chat".ws(|mut socket| async move {
    ///         while let Some(message) = socket.next().await {
    ///             if let Some(text) = message?.into_text() {
    ///                 socket.send_text(text).await?;
    ///             }
    ///         }
    ///         Ok::<_, skyzen::Error>(())
    ///     }),
    /// ));
    /// ```
    #[cfg(feature = "ws")]
    fn ws<F, Fut>(self, session: F) -> RouteNode
    where
        F: Fn(WebSocket) -> Fut + Clone + MaybeSend + MaybeSync + 'static,
        Fut: Future + MaybeSend + 'static,
        Fut::Output: crate::websocket::IntoWebSocketOutcome + 'static,
    {
        self.at(crate::websocket::session_handler(session))
    }
}

impl<P> CreateRouteNode for P
where
    P: Into<String>,
{
    fn at<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::GET, handler)
    }

    fn post<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::POST, handler)
    }

    fn patch<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::PATCH, handler)
    }

    fn put<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::PUT, handler)
    }

    fn delete<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::DELETE, handler)
    }

    fn head<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::HEAD, handler)
    }

    fn options<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::OPTIONS, handler)
    }

    fn trace<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        self.on(Method::TRACE, handler)
    }

    fn on<H, T, R>(self, method: Method, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        endpoint_node_from_handler(self, MethodFilter::Exact(method), handler)
    }

    fn any<H, T, R>(self, handler: H) -> RouteNode
    where
        H: Handler<T, R>,
        T: Extractor,
        R: Responder,
    {
        endpoint_node_from_handler(self, MethodFilter::Any, handler)
    }

    fn endpoint<E>(self, method: Method, endpoint: E) -> RouteNode
    where
        E: Endpoint + Clone + Send + Sync + 'static,
    {
        RouteNode::new_endpoint(
            self,
            MethodFilter::Exact(method),
            endpoint,
            None,
            Vec::new(),
        )
    }

    fn route(self, routes: impl Routes) -> RouteNode {
        RouteNode::new_route(self.into(), Route::new(routes))
    }

    fn nest(self, router: Router) -> RouteNode {
        RouteNode::new_nested(self.into(), router)
    }
}
