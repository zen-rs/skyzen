//! Routing between endpoints
//!
//! Skyzen uses a tree-based routing system. The original `CreateRouteNode::at` builder syntax
//! shown in the examples below is still experimental, so the snippets are marked as ignored:
//! ```ignore
//! use skyzen::{CreateRouteNode, routing::Params, Route};
//! Route::new([
//!     "/home".at(|| async {}),
//!     "/about".at(|| async {}),
//! ]);
//! ```
//!
//! Named parameters are extracted via [`Params`]:
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route};
//! async fn hello(params: Params) -> String {
//!     let name = params.get("name").unwrap();
//!     format!("Hello, {name}!")
//! }
//! Route::new([
//!     "/user/:name".at(hello)
//! ]);
//! ```
//!
//! Catch-all segments after `/*` are also supported:
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route};
//! async fn echo(params: Params) -> String {
//!     let path = params.get("path").unwrap();
//!     format!("Path: {path}")
//! }
//! Route::new([
//!     "/file/*path".at(echo)
//! ]);
//! ```
//!
//! # Applying middleware for routes
//! You can apply middleware with `Route::middleware`:
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route, utils::State};
//! Route::new([
//!     "/counter".at(|| async {})
//! ]).middleware(State(0));
//! ```
//! Middleware applied on a route will also be recursively applied to every child node.
//!
//! # Error handling
//! Capture endpoint failures with [`ErrorHandlingMiddleware`](crate::middleware::ErrorHandlingMiddleware):
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route, utils::State, middleware::ErrorHandlingMiddleware};
//! Route::new([]).middleware(ErrorHandlingMiddleware::new(|error| async { "Error!" }));
//! ```
//! or use the convenience method `error_handling`:
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route, utils::State};
//! Route::new([]).error_handling(|error| async { "Error!" });
//! ```
//!
//! Handlers bubble errors from the innermost route outward until one is caught:
//! ```ignore
//! # use skyzen::{CreateRouteNode, routing::Params, Route, utils::State};
//! Route::new([
//!     "/test".at(|| async {})
//!         .error_handling(|error| async { "Inner error" })
//! ]).error_handling(|error| async { "Outer error" });
//! ```

use http_kit::endpoint::AnyEndpoint;
use http_kit::{Endpoint, Method};

/// Type alias for dynamically dispatched endpoints stored in the routing tree.
pub type BoxEndpoint = AnyEndpoint;
// type SharedMiddleware = Box<dyn Middleware>; // Disabled for now

// Export param types
mod param;
pub use param::Params;

// Export router types
mod router;
pub use router::{Router, RouteBuildError, build};

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
#[derive(Debug)]
pub enum RouteNodeType {
    /// Sub-route with additional child nodes.
    Route(Route),
    /// Terminal endpoint located at the provided path and method.
    Endpoint {
        /// Handler invoked for the route.
        endpoint: BoxEndpoint,
        /// HTTP method matched by the node.
        method: Method,
        // middlewares: Vec<SharedMiddleware>, // Disabled for now
    },
}

impl Route {
    /// Build a [`Route`] from a vector of pre-constructed nodes.
    #[must_use]
    pub const fn new(nodes: Vec<RouteNode>) -> Self {
        Self { nodes }
    }
}

impl RouteNode {
    /// Construct an endpoint node with the provided handler.
    #[must_use]
    pub fn new_endpoint(
        path: impl Into<String>,
        method: Method,
        endpoint: impl Endpoint + 'static,
    ) -> Self {
        Self {
            path: path.into(),
            node_type: RouteNodeType::Endpoint {
                endpoint: AnyEndpoint::new(endpoint),
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
}

// Trait for building routes
/// Trait implemented by types that can be converted into route nodes.
pub trait Routes {
    /// Consume the type and produce the corresponding route nodes.
    fn into_route_nodes(self) -> Vec<RouteNode>;
}

impl Routes for Vec<RouteNode> {
    fn into_route_nodes(self) -> Vec<RouteNode> {
        self
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
