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
mod param;
mod router;
use crate::{handler::into_endpoint, middleware::ErrorHandlingMiddleware, utils::State};
use http_kit::Endpoint;
pub use param::Params;
pub use router::Router;
use skyzen_core::Responder;
use std::{
    fmt::Debug,
    future::Future,
    ops::{Deref, DerefMut},
    sync::Arc,
};

type BoxEndpoint = Box<dyn Endpoint>;

use crate::{extract::Extractor, Method, Middleware};

use crate::handler::Handler;

/// A node of route.
#[derive(Debug)]
pub struct RouteNode {
    path: String,
    node_type: RouteNodeType,
}

impl RouteNode {
    /// Create a route node with method and endpoint.
    pub fn method(path: String, method: Method, endpoint: BoxEndpoint) -> Self {
        Self {
            path,
            node_type: RouteNodeType::Endpoint {
                endpoint,
                method,
                middlewares: Vec::new(),
            },
        }
    }
    /// Create a nest route.
    pub fn route(path: String, route: Route) -> Self {
        Self {
            path,
            node_type: RouteNodeType::Route(route),
        }
    }

    /// Set middleware for this route node.
    /// If this route node is a route, all route nodes contains in this node will be applied this middleware.
    pub fn middleware(mut self, middleware: impl Middleware + 'static) -> Self {
        self.set_middleware(middleware);
        self
    }

    /// Set middleware for this route node.
    /// This method don't require ownership of `RouteNode`
    /// If this route node is a route, all route nodes contains in this node will be applied this middleware.
    pub fn set_middleware(&mut self, middleware: impl Middleware + 'static) {
        self.set_middleware_inner(Arc::new(middleware));
    }

    fn set_middleware_inner(&mut self, middleware: SharedMiddleware) {
        match self.node_type {
            RouteNodeType::Route(ref mut routes) => routes.set_middleware_inner(middleware),
            RouteNodeType::Endpoint {
                ref mut middlewares,
                ..
            } => middlewares.push(middleware),
        }
    }

    /// Share the state of application.
    pub fn state<T: Clone + Send + Sync + 'static>(self, state: T) -> Self {
        self.middleware(State(state))
    }

    /// Handle a error by applying a function.
    pub fn error_handling<F: Send + Sync, Fut: Send, Res>(self, f: F) -> Self
    where
        F: 'static + Fn(crate::Error) -> Fut,
        Fut: Future<Output = Res>,
        Res: Responder,
    {
        self.middleware(ErrorHandlingMiddleware::new(f))
    }
}

enum RouteNodeType {
    Route(Route),
    Endpoint {
        endpoint: BoxEndpoint,
        middlewares: Vec<SharedMiddleware>,
        method: Method,
    },
}

impl Debug for RouteNodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteNodeType::Route(route) => f.debug_list().entry(&route).finish(),
            RouteNodeType::Endpoint { method, .. } => f
                .debug_struct("Endpoint")
                .field("method", &method.to_string())
                .finish(),
        }
    }
}

/// Route is a collection of route nodes.
///
/// See the [`routing`](crate::routing) documentation for the usage of skyzen's routing system.
#[derive(Debug)]
pub struct Route {
    nodes: Vec<RouteNode>,
}

impl Route {
    /// Create a route with route nodes.
    pub fn new(nodes: impl Into<Vec<RouteNode>>) -> Self {
        Self {
            nodes: nodes.into(),
        }
    }
    /// Set middleware for this route.
    /// All route nodes contains in this route will be applied this middleware.
    pub fn middleware(mut self, middleware: impl Middleware + Send + Sync + 'static) -> Self {
        self.set_middleware(middleware);
        self
    }

    /// Set middleware for this route.
    /// This method don't require ownership of `Route`
    /// All route nodes contains in this route will be applied this middleware.
    pub fn set_middleware(&mut self, middleware: impl Middleware + Send + Sync + 'static) {
        self.set_middleware_inner(Arc::new(middleware))
    }

    fn set_middleware_inner(&mut self, middleware: SharedMiddleware) {
        for node in self.nodes.deref_mut() {
            node.set_middleware_inner(middleware.clone())
        }
    }

    /// Share the state of application.
    pub fn state<T: Clone + Send + Sync + 'static>(self, state: T) -> Self {
        self.middleware(State(state))
    }

    /// Handle a error by applying a function.
    pub fn error_handling<F: Send + Sync, Fut: Send, Res>(self, f: F) -> Self
    where
        F: 'static + Fn(crate::Error) -> Fut,
        Fut: Future<Output = Res>,
        Res: Responder,
    {
        self.middleware(ErrorHandlingMiddleware::new(f))
    }

    /// Build this endpoint to endpoint
    // TODO:panic warning
    pub fn build(self) -> impl Endpoint {
        router::build(self).unwrap()
    }
}

impl Deref for Route {
    type Target = [RouteNode];

    fn deref(&self) -> &Self::Target {
        self.nodes.deref()
    }
}

impl DerefMut for Route {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.nodes.deref_mut()
    }
}

impl<T: Into<Vec<RouteNode>>> From<T> for Route {
    fn from(nodes: T) -> Self {
        Self {
            nodes: nodes.into(),
        }
    }
}

mod sealed {
    use std::borrow::Cow;

    pub trait Sealed {}

    impl Sealed for &str {}
    impl<'a> Sealed for Cow<'a, str> {}
    impl Sealed for String {}
}

/// Provide a plenty of methods to create a route node.
pub trait CreateRouteNode: sealed::Sealed + Sized {
    /// Create a route node in a HTTP method.
    fn method<T: Extractor + Send + Sync + 'static>(
        self,
        method: Method,
        handler: impl Handler<T> + Send + Sync + 'static,
    ) -> RouteNode;

    /// Create a GET route node.
    fn at<T: Extractor + Send + Sync + 'static>(
        self,
        handler: impl Handler<T> + Send + Sync + 'static,
    ) -> RouteNode {
        self.method(Method::GET, handler)
    }

    /// Create a POST route node.

    fn post<T: Extractor + Send + Sync + 'static>(
        self,
        handler: impl Handler<T> + Send + Sync + 'static,
    ) -> RouteNode {
        self.method(Method::POST, handler)
    }

    /// Create a PUT route node.

    fn put<T: Extractor + Send + Sync + 'static>(
        self,
        handler: impl Handler<T> + Send + Sync + 'static,
    ) -> RouteNode {
        self.method(Method::PUT, handler)
    }

    /// Create a DELETE route node.

    fn delete<T: Extractor + Send + Sync + 'static>(
        self,
        handler: impl Handler<T> + Send + Sync + 'static,
    ) -> RouteNode {
        self.method(Method::DELETE, handler)
    }
    /// Create a nest route contains a lots of route nodes.
    fn route(self, route: impl Into<Vec<RouteNode>>) -> RouteNode;
}

impl CreateRouteNode for String {
    fn method<T: Extractor + Send + Sync + 'static>(
        self,
        method: Method,
        handler: impl Handler<T> + Send + Sync + 'static,
    ) -> RouteNode {
        RouteNode::method(self, method, Box::new(into_endpoint(handler)))
    }

    fn route(self, route: impl Into<Vec<RouteNode>>) -> RouteNode {
        RouteNode::route(self, Route::from(route))
    }
}

impl CreateRouteNode for &str {
    fn method<T: Extractor + Send + Sync + 'static>(
        self,
        method: Method,
        handler: impl Handler<T> + Send + Sync + 'static,
    ) -> RouteNode {
        self.to_owned().method(method, handler)
    }

    fn route(self, route: impl Into<Vec<RouteNode>>) -> RouteNode {
        self.to_owned().route(route)
    }
}
