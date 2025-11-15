//! Routing between endpoints
//!
//! Skyzen uses a tree-based routing system. The original `CreateRouteNode::at` builder syntax
//! shown in the examples below is still experimental, so the snippets are marked as ignored:
//! ```ignore
//! use skyzen::{CreateRouteNode, routing::Params, Route};
//! Route::new((
//!     "/home".at(|| async {}),
//!     "/about".at(|| async {}),
//! ));
//! ```
//!
//! Named parameters are extracted via [`Params`]:
//! ```ignore
//! use skyzen::{CreateRouteNode, routing::Params, Route};
//! async fn hello(params: Params) -> String {
//!     let name = params.get("name").unwrap();
//!     format!("Hello, {name}!")
//! }
//! Route::new((
//!     "/user/:name".at(hello)
//! ));
//! ```
//!
//! Catch-all segments after `/*` are also supported:
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route};
//! async fn echo(params: Params) -> String {
//!     let path = params.get("path").unwrap();
//!     format!("Path: {path}")
//! }
//! Route::new((
//!     "/file/*path".at(echo)
//! ));
//! ```
//!
//! # Applying middleware for routes
//! You can apply middleware with `Route::middleware`:
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route, utils::State};
//! Route::new((
//!     "/counter".at(|| async {})
//! )).middleware(State(0));
//! ```
//! Middleware applied on a route will also be recursively applied to every child node.
//!
//! # Error handling
//! Capture endpoint failures with [`ErrorHandlingMiddleware`](crate::middleware::ErrorHandlingMiddleware):
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route, utils::State, middleware::ErrorHandlingMiddleware};
//! Route::new(()).middleware(ErrorHandlingMiddleware::new(|error| async { "Error!" }));
//! ```
//! or use the convenience method `error_handling`:
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route, utils::State};
//! Route::new(()).error_handling(|error| async { "Error!" });
//! ```
//!
//! Handlers bubble errors from the innermost route outward until one is caught:
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route, utils::State};
//! Route::new((
//!     "/test".at(|| async {})
//!         .error_handling(|error| async { "Inner error" })
//! )).error_handling(|error| async { "Outer error" });
//! ```

use std::{fmt, sync::Arc};

use crate::Middleware;
use http_kit::endpoint::{AnyEndpoint, WithMiddleware};
use http_kit::{Endpoint, Method};

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
    pub nodes: Vec<RouteNode>,
}

/// A single node in the routing tree.
#[derive(Debug)]
pub struct RouteNode {
    /// The literal path segment represented by this node.
    pub path: String,
    /// The kind of node.
    pub node_type: RouteNodeType,
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
        M: Middleware + Clone + 'static,
    {
        self.apply_middleware(middleware);
        self
    }

    #[allow(clippy::needless_pass_by_value)]
    fn apply_middleware<M>(&mut self, middleware: M)
    where
        M: Middleware + Clone + 'static,
    {
        for node in &mut self.nodes {
            node.apply_middleware(middleware.clone());
        }
    }
}

impl RouteNode {
    /// Construct an endpoint node with the provided handler.
    #[must_use]
    pub fn new_endpoint<E>(path: impl Into<String>, method: Method, endpoint: E) -> Self
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
    pub fn new_route(path: impl Into<String>, route: Route) -> Self {
        Self {
            path: path.into(),
            node_type: RouteNodeType::Route(route),
        }
    }

    fn apply_middleware<M>(&mut self, middleware: M)
    where
        M: Middleware + Clone + 'static,
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
    M: Middleware + Clone + 'static,
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

// Disabled the Node trait for now since middleware system needs redesign
// pub trait Node {
//     fn apply_middleware(
//         self,
//         middleware: impl Middleware + 'static,
//     ) -> impl Node;

//     fn into_endpoints(self) -> Vec<EndpointNode<AnyEndpoint>>;
// }

// Remove the generic Node implementation for now since we have concrete types
