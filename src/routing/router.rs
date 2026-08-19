use std::{
    collections::{HashMap, HashSet},
    fmt::{Debug, Formatter},
    sync::Arc,
};

use super::{BoxEndpoint, EndpointFactory, Params, Route, RouteNode, RouteNodeType};
#[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
use crate::openapi::RouteOpenApiEntry;
use crate::{header, openapi::OpenApi, Body, Endpoint, Method, Request, Response, StatusCode};

use http_kit::error::BoxHttpError;
use http_kit::http_error;
use matchit::Match;
use skyzen_core::Extractor;
use tracing::debug;

// The entrance of request,composing of endpoint
pub struct App {
    endpoint_factory: EndpointFactory,
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
    already_router_enabled: bool,
    /// Optional alarm handler for Durable Object alarm events.
    pub(crate) alarm_handler: Option<EndpointFactory>,
    #[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
    openapi_entries: Arc<Vec<RouteOpenApiEntry>>,
}

impl Debug for Router {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut debug_struct = f.debug_struct("Router");
        debug_struct
            .field("inner", &self.inner)
            .field("already_router_enabled", &self.already_router_enabled)
            .field("has_alarm_handler", &self.alarm_handler.is_some());
        #[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
        {
            debug_struct.field("openapi_entries", &self.openapi_entries.len());
        }
        debug_struct.finish()
    }
}

http_error!(pub NotFound, StatusCode::NOT_FOUND, "Route not found.");

/// Outcome of matching a request against the routing tree.
enum RouteLookup<'app> {
    /// The path and method matched. `head_fallback` marks a HEAD request served
    /// by the GET handler, whose body must be discarded.
    Endpoint {
        app: &'app App,
        head_fallback: bool,
        params: Vec<(String, String)>,
    },
    /// The path matched but no registered method did.
    MethodNotAllowed(Vec<Method>),
    /// No route matched the path.
    NotFound,
}

impl Router {
    fn lookup(&self, path: &str, method: &Method) -> RouteLookup<'_> {
        let Ok(Match { value, params }) = self.inner.at(path) else {
            return RouteLookup::NotFound;
        };

        let found = value
            .iter()
            .find(|(app_method, ..)| app_method == method)
            .map(|(.., app)| (app, false))
            .or_else(|| {
                // A HEAD request without a dedicated handler falls back to the
                // GET handler; the response body is discarded after it runs.
                if *method == Method::HEAD {
                    value
                        .iter()
                        .find(|(app_method, ..)| *app_method == Method::GET)
                        .map(|(.., app)| (app, true))
                } else {
                    None
                }
            });

        found.map_or_else(
            || RouteLookup::MethodNotAllowed(allowed_methods(value)),
            |(app, head_fallback)| RouteLookup::Endpoint {
                app,
                head_fallback,
                params: params
                    .iter()
                    .map(|(key, value)| (key.to_owned(), percent_decode(value)))
                    .collect(),
            },
        )
    }

    async fn call(&self, request: &mut Request) -> Result<Response, BoxHttpError> {
        debug!(
            method = request.method().as_str(),
            path = request.uri().path(),
            "request received"
        );

        if self.already_router_enabled {
            request.extensions_mut().insert(self.clone());
        }

        let method = request.method().clone();
        let lookup = self.lookup(request.uri().path(), &method);

        match lookup {
            RouteLookup::Endpoint {
                app,
                head_fallback,
                params,
            } => {
                request.extensions_mut().insert(Params::new(params));

                let mut endpoint = app.endpoint();
                let mut response = endpoint.respond(request).await?;
                if head_fallback {
                    *response.body_mut() = Body::empty();
                }
                Ok(response)
            }
            RouteLookup::MethodNotAllowed(allow) => Ok(method_not_allowed_response(&allow)),
            RouteLookup::NotFound => Err(Box::new(NotFound::new()) as BoxHttpError),
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
    pub const fn enable_programmable_router(mut self) -> Self {
        self.already_router_enabled = true;
        self
    }

    /// Build an [`OpenApi`] definition containing every route registered on this router.
    #[must_use]
    pub fn openapi(&self) -> OpenApi {
        #[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
        {
            OpenApi::from_entries(&self.openapi_entries)
        }

        #[cfg(not(all(feature = "openapi", not(target_arch = "wasm32"))))]
        {
            OpenApi::default()
        }
    }

    /// Create a fresh alarm endpoint, if one was registered via [`Route::on_alarm`].
    #[must_use]
    pub fn alarm_endpoint(&self) -> Option<BoxEndpoint> {
        self.alarm_handler.as_ref().map(|factory| factory())
    }
}

/// Collect the methods registered for a path, advertising HEAD alongside GET.
fn allowed_methods(entries: &[(Method, App)]) -> Vec<Method> {
    let mut methods: Vec<Method> = entries.iter().map(|(method, ..)| method.clone()).collect();
    if methods.contains(&Method::GET) && !methods.contains(&Method::HEAD) {
        methods.push(Method::HEAD);
    }
    methods
}

/// Build a `405 Method Not Allowed` response advertising the allowed methods.
fn method_not_allowed_response(allow: &[Method]) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    let list = allow
        .iter()
        .map(Method::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if let Ok(value) = header::HeaderValue::from_str(&list) {
        response.headers_mut().insert(header::ALLOW, value);
    }
    response
}

/// Decode percent-encoded octets in a path parameter value.
///
/// Invalid percent sequences or non-UTF-8 results leave the raw value untouched
/// instead of failing the request.
fn percent_decode(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_owned();
    }

    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let high = bytes.get(index + 1).copied().and_then(hex_value);
            let low = bytes.get(index + 2).copied().and_then(hex_value);
            let (Some(high), Some(low)) = (high, low) else {
                return raw.to_owned();
            };
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }

    String::from_utf8(decoded).unwrap_or_else(|_| raw.to_owned())
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

http_error!(pub RouterNotExist, StatusCode::INTERNAL_SERVER_ERROR, "Router not available in request extensions. Call `enable_programmable_router()` when building the route.");

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

impl std::fmt::Display for RouteBuildError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepeatedMethod { path, method } => write!(
                f,
                "method `{method}` is registered multiple times for path `{path}`"
            ),
            Self::MatchitError(error) => write!(f, "invalid route pattern: {error}"),
        }
    }
}

impl std::error::Error for RouteBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MatchitError(error) => Some(error),
            Self::RepeatedMethod { .. } => None,
        }
    }
}

impl From<matchit::InsertError> for RouteBuildError {
    fn from(error: matchit::InsertError) -> Self {
        Self::MatchitError(error)
    }
}

type FlattenBuf = HashMap<String, Vec<(Method, App)>>;

#[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
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

#[cfg(not(all(feature = "openapi", not(target_arch = "wasm32"))))]
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
#[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
pub fn build(route: Route) -> Result<Router, RouteBuildError> {
    let mut buf = HashMap::new();
    let mut openapi_entries = Vec::new();
    flatten("", route.nodes, &mut buf, &mut openapi_entries);
    finalize_router(buf, openapi_entries)
}

/// Build a [`Router`] from a [`Route`] tree.
///
/// # Errors
///
/// Returns [`RouteBuildError`] if the route tree contains conflicting method registrations or if
/// the underlying path matcher rejects the route definition.
#[cfg(not(all(feature = "openapi", not(target_arch = "wasm32"))))]
pub fn build(route: Route) -> Result<Router, RouteBuildError> {
    let mut buf = HashMap::new();
    flatten("", route.nodes, &mut buf);
    finalize_router(buf)
}

fn finalize_router(
    buf: HashMap<String, Vec<(Method, App)>>,
    #[cfg(all(feature = "openapi", not(target_arch = "wasm32")))] openapi_entries: Vec<
        RouteOpenApiEntry,
    >,
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
        already_router_enabled: false,
        alarm_handler: None,
        #[cfg(all(feature = "openapi", not(target_arch = "wasm32")))]
        openapi_entries: Arc::new(openapi_entries),
    })
}

impl Endpoint for Router {
    type Error = BoxHttpError;
    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        // Propagate errors to let middleware or runtime handle them
        self.call(request).await
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
        utils::{Form, Json},
        Body, Error, Method, Response, Result, StatusCode, ToSchema,
    };
    use serde::{Deserialize, Serialize};

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
    async fn returns_method_not_allowed_with_allow_header() {
        let route = Route::new((
            "/items".at(|| async { Result::Ok("list") }),
            "/items".post(|| async { Result::Ok("created") }),
        ));
        let router = build(route).unwrap();

        let request = request_with_method("/items", Method::DELETE);
        let response = router.clone().go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

        let allow = response
            .headers()
            .get(header::ALLOW)
            .expect("Allow header missing")
            .to_str()
            .unwrap();
        let methods: Vec<&str> = allow.split(", ").collect();
        assert!(methods.contains(&"GET"));
        assert!(methods.contains(&"POST"));
        // GET implies HEAD via the fallback, so it must be advertised too.
        assert!(methods.contains(&"HEAD"));
    }

    #[tokio::test]
    async fn head_requests_fall_back_to_get_handler() {
        let route = Route::new(("/doc".at(|| async { Result::Ok("payload") }),));
        let router = build(route).unwrap();

        let request = request_with_method("/doc", Method::HEAD);
        let response = router.clone().go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().into_string().await.unwrap();
        assert!(body.is_empty(), "HEAD response body must be empty");
    }

    #[tokio::test]
    async fn matches_wildcard_routes() {
        async fn echo(params: Params) -> Result<String> {
            Ok(params.get("path")?.to_owned())
        }

        let router = build(Route::new(("/files/{*path}".at(echo),))).unwrap();
        let response = router
            .clone()
            .go(get_request("/files/a/b/c.txt"))
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "a/b/c.txt");
    }

    #[tokio::test]
    async fn prefers_static_segments_over_params() {
        async fn by_id(params: Params) -> Result<String> {
            Ok(format!("id:{}", params.get("id")?))
        }

        let router = build(Route::new((
            "/items/{id}".at(by_id),
            "/items/new".at(|| async { Result::Ok("new-form") }),
        )))
        .unwrap();

        let response = router.clone().go(get_request("/items/new")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "new-form");

        let response = router.clone().go(get_request("/items/42")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "id:42");
    }

    #[tokio::test]
    async fn treats_trailing_slash_as_distinct_route() {
        let router = build(Route::new((
            "/dir".at(|| async { Result::Ok("no-slash") }),
            "/dir/".at(|| async { Result::Ok("slash") }),
        )))
        .unwrap();

        let response = router.clone().go(get_request("/dir")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "no-slash");

        let response = router.clone().go(get_request("/dir/")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "slash");
    }

    #[tokio::test]
    async fn populates_multiple_params() {
        async fn show(params: Params) -> Result<String> {
            Ok(format!("{}/{}", params.get("user")?, params.get("post")?))
        }

        let router = build(Route::new(("/users/{user}/posts/{post}".at(show),))).unwrap();
        let response = router
            .clone()
            .go(get_request("/users/ada/posts/17"))
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "ada/17");
    }

    #[tokio::test]
    async fn percent_decodes_params() {
        async fn greet(params: Params) -> Result<String> {
            Ok(params.get("name")?.to_owned())
        }

        let router = build(Route::new(("/hello/{name}".at(greet),))).unwrap();
        let response = router
            .clone()
            .go(get_request("/hello/John%20Doe"))
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "John Doe");

        // Invalid percent sequences are passed through untouched.
        let response = router
            .clone()
            .go(get_request("/hello/50%25off"))
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "50%off");
    }

    #[tokio::test]
    async fn params_can_be_extracted_multiple_times() {
        async fn greet(first: Params, second: Params) -> Result<String> {
            Ok(format!("{}+{}", first.get("name")?, second.get("name")?))
        }

        let router = build(Route::new(("/hello/{name}".at(greet),))).unwrap();
        let response = router.clone().go(get_request("/hello/Ada")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "Ada+Ada");
    }

    #[tokio::test]
    async fn mounts_nested_routes_under_prefix() {
        let router = build(Route::new((
            "/api".route(("/v1".route(("/ping".at(|| async { Result::Ok("pong") }),)),)),
        )))
        .unwrap();

        let response = router
            .clone()
            .go(get_request("/api/v1/ping"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "pong");
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

        // `.with` is an alias for `.middleware` and must behave identically.
        let aliased = build(
            Route::new(("/ping".at(|| async { Result::Ok("pong") }),)).with(HeaderMiddleware),
        )
        .unwrap();
        let response = aliased.clone().go(get_request("/ping")).await.unwrap();
        assert!(response.headers().get("x-middleware").is_some());
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
    async fn error_handling_middleware_preserves_error_status() {
        async fn fail() -> Result<&'static str> {
            Err(Error::msg("teapot").set_status(StatusCode::IM_A_TEAPOT))
        }

        let route = Route::new(("/fail".at(fail),)).middleware(ErrorHandlingMiddleware::new(
            |error| async move { format!("handled: {error}") },
        ));

        let router = build(route).unwrap();
        let response = router.clone().go(get_request("/fail")).await.unwrap();
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
        let body = response.into_body().into_string().await.unwrap();
        assert!(body.starts_with("handled:"));
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
    async fn routes_extended_http_method_builders() {
        let router = build(Route::new((
            "/items".patch(|| async { Result::Ok("patched") }),
            "/items".head(|| async { Result::Ok("") }),
            "/items".options(|| async { Result::Ok("options") }),
            "/items".trace(|| async { Result::Ok("trace") }),
        )))
        .unwrap();

        for (method, expected) in [
            (Method::PATCH, "patched"),
            (Method::HEAD, ""),
            (Method::OPTIONS, "options"),
            (Method::TRACE, "trace"),
        ] {
            let request = request_with_method("/items", method);
            let response = router.clone().go(request).await.unwrap();
            let body = response.into_body().into_string().await.unwrap();
            assert_eq!(body, expected);
        }
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

    #[derive(Debug, Deserialize, ToSchema)]
    struct CreateWidget {
        name: String,
    }

    #[derive(Debug, Serialize, ToSchema)]
    struct Widget {
        id: String,
        name: String,
    }

    #[skyzen::openapi]
    async fn create_widget(Json(body): Json<CreateWidget>) -> Result<Json<Widget>> {
        Ok(Json(Widget {
            id: "widget-1".into(),
            name: body.name,
        }))
    }

    #[test]
    fn openapi_collects_typed_request_and_response_schemas() {
        let openapi = Route::new(("/widgets".post(create_widget),)).openapi();
        assert!(openapi.is_enabled());

        let operation = openapi
            .operations()
            .iter()
            .find(|operation| operation.path == "/widgets" && operation.method == Method::POST)
            .expect("expected POST /widgets operation");

        assert!(
            operation
                .parameters
                .iter()
                .any(|parameter| parameter.schema.schema.is_some()),
            "expected documented request schema"
        );
        assert!(
            operation
                .responses
                .iter()
                .any(|response| response.schema.is_some()),
            "expected documented response schema"
        );
    }

    #[derive(Debug, Deserialize, Serialize, ToSchema)]
    struct LoginFormPayload {
        user: String,
        remember: bool,
    }

    #[skyzen::openapi]
    async fn submit_form(Form(body): Form<LoginFormPayload>) -> Result<Form<LoginFormPayload>> {
        Ok(Form(body))
    }

    #[test]
    fn openapi_collects_typed_form_request_and_response_schemas() {
        let openapi = Route::new(("/login".post(submit_form),)).openapi();
        assert!(openapi.is_enabled());

        let operation = openapi
            .operations()
            .iter()
            .find(|operation| operation.path == "/login" && operation.method == Method::POST)
            .expect("expected POST /login operation");

        assert!(
            operation.parameters.iter().any(|parameter| {
                parameter.schema.content_type == Some("application/x-www-form-urlencoded")
                    && parameter.schema.schema.is_some()
            }),
            "expected documented form request schema"
        );
        assert!(
            operation.responses.iter().any(|response| {
                response.content_type == Some("application/x-www-form-urlencoded")
                    && response.schema.is_some()
            }),
            "expected documented form response schema"
        );
    }

    #[derive(Debug, Deserialize, ToSchema)]
    struct ListParams {
        page: u32,
        tag: Option<String>,
    }

    #[skyzen::openapi]
    async fn list_items(
        query: crate::extract::Query<ListParams>,
        _params: Params,
    ) -> Result<String> {
        Ok(format!("page {} tag {:?}", query.0.page, query.0.tag))
    }

    #[test]
    fn openapi_emits_query_and_path_parameters_not_request_body() {
        let spec = Route::new(("/items/{id}".at(list_items),))
            .openapi()
            .to_utoipa_spec();
        let json = serde_json::to_value(&spec).expect("serialize spec");
        let operation = &json["paths"]["/items/{id}"]["get"];

        let parameters = operation["parameters"]
            .as_array()
            .expect("operation should declare parameters");
        let find = |name: &str| parameters.iter().find(|param| param["name"] == name);

        let id_param = find("id").expect("path parameter `id`");
        assert_eq!(id_param["in"], "path");
        assert_eq!(id_param["required"], true);

        let page = find("page").expect("query parameter `page`");
        assert_eq!(page["in"], "query");
        assert_eq!(page["required"], true);

        let tag = find("tag").expect("query parameter `tag`");
        assert_eq!(tag["in"], "query");
        assert_eq!(tag["required"], false);

        assert!(
            operation["requestBody"].is_null(),
            "a GET with query/path params must not declare a request body"
        );
    }
}
