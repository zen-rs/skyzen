//! Routing between endpoints
//!
//! Skyzen use tree-based routing system, you can create a [`RouteNode`](crate::routing::RouteNode) with [`CreateRouteNode`](crate::routing::CreateRouteNode) trait, just like this:
//! ```
//! use skyzen::{CreateRouteNode,routing::Params,Route};
//! Route::new([
//!     "/home".at(||async{}),
//!     "/about".at(||async{}),
//! ]);
//! ```
//! What's more, you can use named parameters in route.
//! ```
//! # use skyzen::{CreateRouteNode,routing::Params,Route};
//! async fn hello(params:Params) -> String{
//!     let name=params.get("name").unwrap();
//!     format!("Hello,{name}!")
//! }
//! Route::new([
//!     "/user/:name".at(hello)
//! ]);
//! ```
//!
//! Or...match everything after `/*` with catch-all parameters!
//!```
//! # use skyzen::{CreateRouteNode,routing::Params,Route};
//! async fn echo(params:Params) -> String{
//!     let path=params.get("path").unwrap();
//!     format!("Path: {path}")
//! }
//! Route::new([
//!     "/file/*path".at(echo)
//! ]);
//!```
//! # Applying middleware for routes
//! You can apply middleware for routes with `middleware` method of `Route` or `RouteNode`
//! ```
//! # use skyzen::{CreateRouteNode,routing::Params,Route,utils::State};
//! Route::new([
//!     "/couter".at(||async{})
//! ]).middleware(State(0));
//! ```
//! Middleware applied on a route (both including `Route` and `RouteNode` represented a route) will also be recursively applied on all the route node in the route.
//!
//! # Error handling
//! You can catch the error when endpoint fails with [`ErrorHandlingMiddleware`](crate::middleware::ErrorHandlingMiddleware).
//! ```
//! # use skyzen::{CreateRouteNode,routing::Params,Route,utils::State,middleware::ErrorHandlingMiddleware};
//! Route::new([
//!
//! ]).middleware(ErrorHandlingMiddleware::new(|error| async{"Error!"}));
//! ```
//! or use the convenience method `error_handling`
//! ```
//! # use skyzen::{CreateRouteNode,routing::Params,Route,utils::State};
//! Route::new([
//!
//! ]).error_handling(|error| async{"Error!"});
//! ```
//!
//! The handling function will be called from inside to outside, until the error is catched and a response is provided.
//! If the error is still not catched even if the outermost handling function is called, skyzen will close the connection and print log.
//!
//! ```
//! # use skyzen::{CreateRouteNode,routing::Params,Route,utils::State};
//! Route::new([
//!     "/test".at(||async{})
//!         .error_handling(|error| async{"Error!"})
//! ]).error_handling(|error| async{"Error!"});
//! ```

use http_kit::endpoint::AnyEndpoint;
use http_kit::{Endpoint, Method};

// Type aliases for the routing system
// Note: These traits are not dyn compatible, so we'll need a different approach
// For now, let's use a simpler system
pub type BoxEndpoint = AnyEndpoint;
// type SharedMiddleware = Box<dyn Middleware>; // Disabled for now

// Export param types
mod param;
pub use param::Params;

// Export router types
mod router;
pub use router::{Router, RouteBuildError, build};

pub struct EndpointNode<E> {
    path: String,
    method: Method,
    endpoint: E,
}

impl<E: Endpoint> EndpointNode<E> {
    /// Create a new route node with the path and endpoint.
    pub fn new(path: impl Into<String>, method: Method, endpoint: E) -> Self {
        Self {
            path: path.into(),
            method,
            endpoint,
        }
    }
}

// Core routing types
#[derive(Debug)]
pub struct Route {
    pub nodes: Vec<RouteNode>,
}

#[derive(Debug)]
pub struct RouteNode {
    pub path: String,
    pub node_type: RouteNodeType,
}

#[derive(Debug)]
pub enum RouteNodeType {
    Route(Route),
    Endpoint {
        endpoint: BoxEndpoint,
        method: Method,
        // middlewares: Vec<SharedMiddleware>, // Disabled for now
    },
}

impl Route {
    pub fn new(nodes: Vec<RouteNode>) -> Self {
        Self { nodes }
    }
}

impl RouteNode {
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
    
    pub fn new_route(path: impl Into<String>, route: Route) -> Self {
        Self {
            path: path.into(),
            node_type: RouteNodeType::Route(route),
        }
    }
}

// Trait for building routes
pub trait Routes {
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
