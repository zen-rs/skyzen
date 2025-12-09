use std::{
    collections::{HashMap, HashSet},
    fmt::{Debug, Formatter},
    sync::Arc,
};

use super::{BoxEndpoint, EndpointFactory, Params, Route, RouteNode, RouteNodeType};
#[cfg(all(debug_assertions, feature = "openapi"))]
use crate::openapi::RouteOpenApiEntry;
use crate::{openapi::OpenApi, Endpoint, Method, Request, Response, StatusCode};

use http_kit::error::BoxHttpError;
use http_kit::http_error;
use matchit::Match;
use skyzen_core::Extractor;
use tracing::{error, info};

// The entrance of request,composing of endpoint
pub struct App {
    endpoint_factory: EndpointFactory,
    // middlewares: SmallVec<[SharedMiddleware; 5]>, // Simplified for now
}

impl Debug for App {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").finish_non_exhaustive()
    }
}

impl App {
    fn new(endpoint_factory: EndpointFactory) -> Self {
        Self { endpoint_factory }
    }

    fn endpoint(&self) -> BoxEndpoint {
        (self.endpoint_factory)()
    }
}

/// An HTTP router returned by [`Route::build`](crate::routing::Route::build).
///
/// `Router` stores its routing tree inside an [`Arc`], so it can be cloned cheaply and shared
/// across threads.
///
/// ```
/// use skyzen::{routing::{CreateRouteNode, Route, Router}, Result};
///
/// let router: Router = Route::new((
///     "/ping".at(|| async { Result::Ok("pong") }),
/// ))
/// .build();
///
/// // Later, inside an async context you can drive the router directly:
/// // let response = router.clone().go(request).await?;
/// ```
#[derive(Clone)]
pub struct Router {
    inner: Arc<matchit::Router<Vec<(Method, App)>>>,
    programmable_router_enabled: bool,
    #[cfg(all(debug_assertions, feature = "openapi"))]
    openapi_entries: Arc<Vec<RouteOpenApiEntry>>,
}

impl Debug for Router {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut debug_struct = f.debug_struct("Router");
        debug_struct.field("inner", &self.inner).field(
            "programmable_router_enabled",
            &self.programmable_router_enabled,
        );
        #[cfg(all(debug_assertions, feature = "openapi"))]
        {
            debug_struct.field("openapi_entries", &self.openapi_entries.len());
        }
        debug_struct.finish()
    }
}

http_error!(pub NotFound, StatusCode::NOT_FOUND, "Route not found.");

#[derive(Debug, Clone, Copy)]
struct NotFoundEndpoint;

impl Endpoint for NotFoundEndpoint {
    type Error = BoxHttpError;
    async fn respond(&mut self, _request: &mut Request) -> Result<Response, Self::Error> {
        Err(Box::new(NotFound::new()) as BoxHttpError)
    }
}

impl Router {
    fn search<'app, 'path, 'temp>(
        &'app self,
        path: &'path str,
        method: &'temp Method,
    ) -> Option<Match<'app, 'path, &'app App>>
    where
        'app: 'path,
        'app: 'temp,
    {
        if let Ok(Match { value, params }) = self.inner.at(path) {
            value
                .iter()
                .find(|(app_method, ..)| app_method == method)
                .map(|(.., app)| Match { value: app, params })
        } else {
            None
        }
    }

    async fn call(&self, request: &mut Request) -> Result<Response, BoxHttpError> {
        if self.programmable_router_enabled {
            request.extensions_mut().insert(self.clone());
        }

        let path = request.uri().path();
        let method = request.method();

        if let Some(Match { value, params }) = self.search(path, method) {
            let params: Vec<(String, String)> = params
                .iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            let params = Params::new(params);
            request.extensions_mut().insert(params);

            let mut endpoint = value.endpoint();
            endpoint.respond(request).await
        } else {
            let mut not_found = NotFoundEndpoint;
            not_found.respond(request).await
        }
    }

    /// Dispatch the provided [`Request`] through the router and return the produced [`Response`].
    ///
    /// # Errors
    ///
    /// Returns any error bubbled up by the matched endpoint, such as rejections from middleware.
    ///
    /// Cloning a router is cheap, so prefer `router.clone().go(request)` when invoking it from
    /// tests or asynchronous workers.
    pub async fn go(&self, mut request: Request) -> Result<Response, BoxHttpError> {
        self.call(&mut request).await
    }

    /// Enable extraction of the current router through [`Extractor`](skyzen_core::Extractor).
    ///
    /// When enabled, the router instance is stored in the request extensions for each call and can
    /// be retrieved inside handlers via `Router::extract(request).await`.
    #[must_use]
    pub const fn enable_programable_router(mut self) -> Self {
        self.programmable_router_enabled = true;
        self
    }

    /// Build an [`OpenApi`] definition containing every route registered on this router.
    #[must_use]
    pub fn openapi(&self) -> OpenApi {
        #[cfg(all(debug_assertions, feature = "openapi"))]
        {
            OpenApi::from_entries(&self.openapi_entries)
        }

        #[cfg(not(all(debug_assertions, feature = "openapi")))]
        {
            OpenApi::default()
        }
    }
}

http_error!(pub RouterNotExist, StatusCode::INTERNAL_SERVER_ERROR, "This programmable router does not exist. Please check whether you have enabled the programmable router.");

impl Extractor for Router {
    type Error = RouterNotExist;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let router = request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(RouterNotExist::new())?;
        Ok(router)
    }
}

/// Errors produced when constructing a [`Router`] from a [`Route`](crate::routing::Route).
#[derive(Debug)]
#[non_exhaustive]
pub enum RouteBuildError {
    /// The same method has been registered multiple times for the same path.
    RepeatedMethod {
        /// Path that already has a handler registered.
        path: String,
        /// Conflicting HTTP method.
        method: Method,
    },
    /// The underlying `matchit` router rejected the provided path pattern.
    MatchitError(matchit::InsertError),
}

impl From<matchit::InsertError> for RouteBuildError {
    fn from(error: matchit::InsertError) -> Self {
        Self::MatchitError(error)
    }
}

type FlattenBuf = HashMap<String, Vec<(Method, App)>>;

#[cfg(all(debug_assertions, feature = "openapi"))]
fn flatten(
    path_prefix: &str,
    route: Vec<RouteNode>,
    buf: &mut FlattenBuf,
    openapi_entries: &mut Vec<RouteOpenApiEntry>,
) {
    for node in route {
        let path = format!("{}{}", path_prefix, node.path);

        match node.node_type {
            RouteNodeType::Route(route) => {
                flatten(&path, route.nodes, buf, openapi_entries);
            }
            RouteNodeType::Endpoint {
                endpoint_factory,
                method,
                openapi,
                // middlewares, // Disabled for now
            } => {
                let entry = buf.entry(path.clone()).or_default();

                entry.push((method.clone(), App::new(endpoint_factory)));
                if let Some(openapi) = openapi {
                    openapi_entries.push(RouteOpenApiEntry::new(path, method, openapi));
                }
            }
        }
    }
}

#[cfg(not(all(debug_assertions, feature = "openapi")))]
fn flatten(path_prefix: &str, route: Vec<RouteNode>, buf: &mut FlattenBuf) {
    for node in route {
        let path = format!("{}{}", path_prefix, node.path);

        match node.node_type {
            RouteNodeType::Route(route) => {
                flatten(&path, route.nodes, buf);
            }
            RouteNodeType::Endpoint {
                endpoint_factory,
                method,
                openapi: _,
                // middlewares, // Disabled for now
            } => {
                let entry = buf.entry(path).or_default();
                entry.push((method, App::new(endpoint_factory)));
            }
        }
    }
}

/// Build a [`Router`] from the provided [`Route`].
///
/// # Errors
///
/// Returns [`RouteBuildError`] if the route tree contains conflicting method registrations or if
/// the underlying path matcher rejects the route definition.
#[cfg(all(debug_assertions, feature = "openapi"))]
pub fn build(route: Route) -> Result<Router, RouteBuildError> {
    let mut buf = HashMap::new();
    let mut openapi_entries = Vec::new();
    flatten("", route.nodes, &mut buf, &mut openapi_entries);
    finalize_router(buf, Some(openapi_entries))
}

/// Build a [`Router`] from a [`Route`] tree.
///
/// # Errors
///
/// Returns [`RouteBuildError`] if the route tree contains conflicting method registrations or if
/// the underlying path matcher rejects the route definition.
#[cfg(not(all(debug_assertions, feature = "openapi")))]
pub fn build(route: Route) -> Result<Router, RouteBuildError> {
    let mut buf = HashMap::new();
    flatten("", route.nodes, &mut buf);
    finalize_router(buf, None)
}

#[cfg(all(debug_assertions, feature = "openapi"))]
fn finalize_router(
    buf: HashMap<String, Vec<(Method, App)>>,
    openapi_entries: Option<Vec<RouteOpenApiEntry>>,
) -> Result<Router, RouteBuildError> {
    let mut router = matchit::Router::new();
    for (path, value) in buf {
        let mut set = HashSet::new();
        for (method, ..) in &value {
            if !set.insert(method) {
                return Err(RouteBuildError::RepeatedMethod {
                    path,
                    method: method.clone(),
                });
            }
        } //check route
        router.insert(path, value)?;
    }
    Ok(Router {
        inner: Arc::new(router),
        programmable_router_enabled: false,
        openapi_entries: Arc::new(openapi_entries.unwrap_or_default()),
    })
}

#[cfg(not(all(debug_assertions, feature = "openapi")))]
fn finalize_router(
    buf: HashMap<String, Vec<(Method, App)>>,
    _openapi_entries: Option<Vec<()>>,
) -> Result<Router, RouteBuildError> {
    let mut router = matchit::Router::new();
    for (path, value) in buf {
        let mut set = HashSet::new();
        for (method, ..) in &value {
            if !set.insert(method) {
                return Err(RouteBuildError::RepeatedMethod {
                    path,
                    method: method.clone(),
                });
            }
        } //check route
        router.insert(path, value)?;
    }
    Ok(Router {
        inner: Arc::new(router),
        programmable_router_enabled: false,
    })
}

impl Endpoint for Router {
    type Error = BoxHttpError;
    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        info!(
            method = request.method().as_str(),
            path = request.uri().path(),
            "request received"
        );
        Ok(self.call(request).await.unwrap_or_else(|error| {
            let mut response = Response::new(http_kit::Body::empty());
            let status = error.status();
            *response.status_mut() = status;
            let error_name = if status.is_server_error() {
                "Server Error"
            } else if status.is_client_error() {
                "Client Error"
            } else {
                "Error"
            };
            error!(
                message = error.to_string().as_str(),
                status = status.as_str(),
                "{error_name}"
            );
            response
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{build, RouteBuildError};
    use crate::{
        header,
        middleware::ErrorHandlingMiddleware,
        middleware::Middleware,
        routing::{CreateRouteNode, Params, Route},
        Body, Error, Method, Response, Result, StatusCode,
    };

    fn get_request(path: &str) -> http_kit::Request {
        request_with_method(path, Method::GET)
    }

    fn request_with_method(path: &str, method: Method) -> http_kit::Request {
        let mut request = http_kit::Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("invalid path");
        *request.method_mut() = method;
        request
    }

    #[tokio::test]
    async fn routes_requests_and_populates_params() {
        async fn greet(params: Params) -> Result<String> {
            let name = params.get("name")?.to_owned();
            Ok(format!("Hello, {name}!"))
        }

        let route = Route::new(("/hello/{name}".at(greet),));
        let router = build(route).unwrap();
        let request = get_request("/hello/Ada");
        let response = router.clone().go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "Hello, Ada!");
    }

    #[tokio::test]
    async fn builds_routes_from_create_route_node_trait() {
        async fn greet(params: Params) -> Result<String> {
            let name = params.get("name")?.to_owned();
            Ok(format!("Hello, {name}!"))
        }

        let route = Route::new(("/hello/{name}".at(greet),));
        let router = build(route).unwrap();
        let request = get_request("/hello/Bob");
        let response = router.clone().go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "Hello, Bob!");
    }

    #[derive(Clone, Default)]
    struct HeaderMiddleware;

    impl Middleware for HeaderMiddleware {
        type Error = std::convert::Infallible;
        async fn handle<N: crate::Endpoint>(
            &mut self,
            request: &mut crate::Request,
            mut next: N,
        ) -> std::result::Result<
            Response,
            http_kit::middleware::MiddlewareError<N::Error, Self::Error>,
        > {
            let mut response = next
                .respond(request)
                .await
                .map_err(http_kit::middleware::MiddlewareError::Endpoint)?;
            response.headers_mut().insert(
                header::HeaderName::from_static("x-middleware"),
                header::HeaderValue::from_static("applied"),
            );
            Ok(response)
        }
    }

    #[tokio::test]
    async fn applies_route_middleware_to_endpoints() {
        let route =
            Route::new(("/ping".at(|| async { Result::Ok("pong") }),)).middleware(HeaderMiddleware);

        let router = build(route).unwrap();
        let request = get_request("/ping");
        let response = router.clone().go(request).await.unwrap();
        let header = response
            .headers()
            .get("x-middleware")
            .expect("header missing");
        assert_eq!(header.to_str().unwrap(), "applied");
    }

    #[tokio::test]
    async fn wraps_handlers_with_error_handling_middleware() {
        async fn fail() -> Result<&'static str> {
            Err(Error::msg("boom"))
        }

        let route = Route::new(("/fail".at(fail),)).middleware(ErrorHandlingMiddleware::new(
            |error| async move { format!("handled: {error}") },
        ));

        let router = build(route).unwrap();
        let request = get_request("/fail");
        let response = router.clone().go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "handled: boom");
    }

    #[tokio::test]
    async fn prevents_duplicate_methods() {
        let route = Route::new((
            "/dup".at(|| async { Result::Ok("first") }),
            "/dup".at(|| async { Result::Ok("second") }),
        ));
        let error = build(route).unwrap_err();
        assert!(matches!(
            error,
            RouteBuildError::RepeatedMethod { path, method }
            if path == "/dup" && method == Method::GET
        ));
    }

    #[tokio::test]
    async fn routes_distinct_methods_on_same_path() {
        async fn list() -> Result<&'static str> {
            Ok("list")
        }

        async fn create() -> Result<&'static str> {
            Ok("created")
        }

        let route = Route::new(("/items".at(list), "/items".post(create)));
        let router = build(route).unwrap();

        let response = router.clone().go(get_request("/items")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "list");

        let request = request_with_method("/items", Method::POST);
        let response = router.clone().go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "created");
    }

    #[tokio::test]
    async fn chains_handlers_on_route_node() {
        async fn list() -> Result<&'static str> {
            Ok("list")
        }

        async fn create() -> Result<&'static str> {
            Ok("created")
        }

        let router = build(Route::new(("/items".at(list).post(create),))).unwrap();

        let response = router.clone().go(get_request("/items")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "list");

        let request = request_with_method("/items", Method::POST);
        let response = router.clone().go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "created");
    }

    #[tokio::test]
    async fn exposes_api_docs_at_root() {
        async fn ping() -> Result<&'static str> {
            Ok("pong")
        }

        let router = build(Route::new(("/ping".at(ping),)).enable_api_doc()).unwrap();

        let response = router.clone().go(get_request("/api-docs")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content type missing")
            .to_str()
            .unwrap();
        assert!(content_type.contains("text/html"));
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn websocket_routes_require_upgrades() {
        use crate::header::{self, HeaderValue};

        let route = Route::new(("/ws".ws(|_socket| async move {}),));
        let router = build(route).unwrap();
        let mut request = get_request("/ws");
        {
            let headers = request.headers_mut();
            headers.insert(
                header::SEC_WEBSOCKET_KEY,
                HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
            );
            headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
            headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
            headers.insert(
                header::SEC_WEBSOCKET_VERSION,
                HeaderValue::from_static("13"),
            );
        }

        let error = router.clone().go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::UPGRADE_REQUIRED);
    }

    #[tokio::test]
    async fn returns_not_found_for_missing_routes() {
        let router = build(Route::new(())).unwrap();
        let request = get_request("/unknown");
        let response = router.clone().go(request).await;
        let error = response.unwrap_err();
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
    }
}
