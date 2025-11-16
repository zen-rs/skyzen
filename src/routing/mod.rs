//! Tree-based routing primitives.
//!
//! Routes are defined by combining nodes produced by the [`CreateRouteNode`] extension. Path
//! literals gain builder methods such as `.at(handler)` (GET), `.post(handler)`, `.put(handler)`,
//! `.delete(handler)`, `.ws(handler)`, and `.route(children)`
//! so you can describe the full tree declaratively. Once a tree is assembled, call [`Route::build`]
//! to obtain a [`Router`] that can be mounted on a server or invoked directly from tests.
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
//! ## Named parameters and wildcards
//! Use `{name}` to capture a single path segment and `{*path}` to capture the rest of the path.
//! Extract the captured values with [`Params`]:
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
//! ## Applying middleware to a route tree
//! Middleware can be attached to a [`Route`] via [`Route::middleware`]. The middleware is cloned
//! for every endpoint reachable from the route:
//! ```no_run
//! use skyzen::{
//!     routing::{CreateRouteNode, Route},
//!     utils::State,
//! };
//!
//! let route = Route::new(("/counter".at(|| async { http_kit::Result::Ok("0") }),))
//! .middleware(State(0usize));
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
//! .middleware(ErrorHandlingMiddleware::new(|error| async move {
//!     format!("Recovered from {error}")
//! }))
//! .build();
//!
//! ## WebSockets
//! When the `websocket` feature is enabled you can use `.ws` to accept upgrades without manually
//! extracting [`WebSocketUpgrade`](crate::websocket::WebSocketUpgrade):
//! ```no_run
//! use futures_util::{SinkExt, StreamExt};
//! use skyzen::{
//!     routing::CreateRouteNode,
//!     websocket::WebSocketMessage,
//! };
//!
//! let routes = Route::new((
//!     "/chat".ws(|mut socket| async move {
//!         while let Some(Ok(message)) = socket.next().await {
//!             if let Ok(text) = message.into_text() {
//!                 let _ = socket.send(WebSocketMessage::text(text)).await;
//!             }
//!         }
//!     }),
//! ));
//! ```
//! The `.ws` builder enforces the HTTP upgrade requirements automatically.
//! ```
//!
//! Middleware is applied from the outermost route to the innermost endpoint, so errors bubble up
//! until they are handled.

#[cfg(feature = "websocket")]
use std::future::Future;
use std::{fmt, sync::Arc};

#[cfg(feature = "websocket")]
use crate::websocket::{WebSocket, WebSocketUpgrade};
use crate::{handler, handler::Handler, Middleware};
use http_kit::endpoint::{AnyEndpoint, WithMiddleware};
use http_kit::{Endpoint, Method};
use skyzen_core::Extractor;

/// Type alias for dynamically dispatched endpoints stored in the routing tree.
pub type BoxEndpoint = AnyEndpoint;
pub(crate) type EndpointFactory = Arc<dyn Fn() -> BoxEndpoint + Send + Sync>;
// type SharedMiddleware = Box<dyn Middleware>; // Disabled for now

// Export param types
mod param;
pub use param::Params;

// Export router types
mod router;
pub use router::{build, RouteBuildError, Router};

/// Collection of route nodes anchored at a path prefix.
#[derive(Debug)]
pub struct Route {
    /// All nodes that hang off the route's mount point.
    nodes: Vec<RouteNode>,
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
        /// HTTP method matched by the node.
        method: Method,
    },
}

impl fmt::Debug for RouteNodeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Route(route) => f.debug_tuple("Route").field(route).finish(),
            Self::Endpoint { method, .. } => {
                f.debug_struct("Endpoint").field("method", method).finish()
            }
        }
    }
}

impl Route {
    /// Build a [`Route`] from the provided nodes.
    #[must_use]
    pub fn new(nodes: impl Routes) -> Self {
        Self {
            nodes: nodes.into_route_nodes(),
        }
    }

    /// Attach middleware to this route and all nested endpoints.
    #[must_use]
    pub fn middleware<M>(mut self, middleware: M) -> Self
    where
        M: Middleware + Sync + Clone + 'static,
    {
        self.apply_middleware(middleware);
        self
    }

    #[allow(clippy::needless_pass_by_value)]
    fn apply_middleware<M>(&mut self, middleware: M)
    where
        M: Middleware + Sync + Clone + 'static,
    {
        for node in &mut self.nodes {
            node.apply_middleware(middleware.clone());
        }
    }

    /// Build the route, panicking on error.
    ///
    /// # Panics
    /// Panics if the route is invalid.
    #[must_use]
    pub fn build(self) -> Router {
        build(self).expect("Failed to build router")
    }
}

impl RouteNode {
    /// Construct an endpoint node with the provided handler.
    #[must_use]
    pub(crate) fn new_endpoint<E>(path: impl Into<String>, method: Method, endpoint: E) -> Self
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
                // middlewares: Vec::new(), // Disabled for now
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

    fn apply_middleware<M>(&mut self, middleware: M)
    where
        M: Middleware + Sync + Clone + 'static,
    {
        match &mut self.node_type {
            RouteNodeType::Route(route) => route.apply_middleware(middleware),
            RouteNodeType::Endpoint {
                endpoint_factory, ..
            } => {
                let factory = Arc::clone(endpoint_factory);
                *endpoint_factory = wrap_endpoint_factory(factory, middleware);
            }
        }
    }
}

fn wrap_endpoint_factory<M>(factory: EndpointFactory, middleware: M) -> EndpointFactory
where
    M: Middleware + Sync + Clone + 'static,
{
    Arc::new(move || {
        let endpoint = factory();
        let middleware = middleware.clone();
        AnyEndpoint::new(WithMiddleware::new(endpoint, middleware))
    })
}

// Trait for building routes
/// Trait implemented by types that can be converted into route nodes.
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
        self.nodes
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

fn endpoint_node_from_handler<P, H, T>(path: P, method: Method, handler: H) -> RouteNode
where
    P: Into<String>,
    H: Handler<T>,
    T: Extractor,
{
    let endpoint = handler::into_endpoint(handler);
    RouteNode::new_endpoint(path.into(), method, endpoint)
}

/// Builder extension that turns a path literal into convenient routing nodes.
pub trait CreateRouteNode: Sized {
    /// Attach a GET handler to the path.
    fn at<H, T>(self, handler: H) -> RouteNode
    where
        H: Handler<T>,
        T: Extractor;

    /// Alias for [`CreateRouteNode::at`].
    fn get<H, T>(self, handler: H) -> RouteNode
    where
        H: Handler<T>,
        T: Extractor,
    {
        self.at(handler)
    }

    /// Attach a POST handler to the path.
    fn post<H, T>(self, handler: H) -> RouteNode
    where
        H: Handler<T>,
        T: Extractor;

    /// Attach a PUT handler to the path.
    fn put<H, T>(self, handler: H) -> RouteNode
    where
        H: Handler<T>,
        T: Extractor;

    /// Attach a DELETE handler to the path.
    fn delete<H, T>(self, handler: H) -> RouteNode
    where
        H: Handler<T>,
        T: Extractor;

    /// Mount nested routes under the current path segment.
    fn route(self, routes: impl Routes) -> RouteNode;

    /// Attach a WebSocket handler that automatically performs the upgrade handshake.
    #[cfg(feature = "websocket")]
    fn ws<F, Fut>(self, handler: F) -> RouteNode
    where
        F: Fn(WebSocket) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let builder = move |upgrade: WebSocketUpgrade| {
            let callback = handler.clone();
            async move { upgrade.on_upgrade(callback) }
        };
        self.at(builder)
    }
}

impl<P> CreateRouteNode for P
where
    P: Into<String>,
{
    fn at<H, T>(self, handler: H) -> RouteNode
    where
        H: Handler<T>,
        T: Extractor,
    {
        endpoint_node_from_handler(self, Method::GET, handler)
    }

    fn post<H, T>(self, handler: H) -> RouteNode
    where
        H: Handler<T>,
        T: Extractor,
    {
        endpoint_node_from_handler(self, Method::POST, handler)
    }

    fn put<H, T>(self, handler: H) -> RouteNode
    where
        H: Handler<T>,
        T: Extractor,
    {
        endpoint_node_from_handler(self, Method::PUT, handler)
    }

    fn delete<H, T>(self, handler: H) -> RouteNode
    where
        H: Handler<T>,
        T: Extractor,
    {
        endpoint_node_from_handler(self, Method::DELETE, handler)
    }

    fn route(self, routes: impl Routes) -> RouteNode {
        RouteNode::new_route(self.into(), Route::new(routes))
    }
}

// Disabled the Node trait for now since middleware system needs redesign
// pub trait Node {
//     fn apply_middleware(
//         self,
//         middleware: impl Middleware + 'static,
//     ) -> impl Node;

//     fn into_endpoints(self) -> Vec<EndpointNode<AnyEndpoint>>;
// }

// Remove the generic Node implementation for now since we have concrete types
