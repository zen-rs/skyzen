use core::future::{ready, Future};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    fmt::{Debug, Formatter},
    sync::Arc,
};

use super::{
    join_path, BoxEndpoint, EndpointFactory, MethodFilter, NestedRouter, Params, Route, RouteNode,
    RouteNodeType,
};
#[cfg(feature = "openapi")]
use crate::openapi::RouteOpenApiEntry;
use crate::{header, openapi::OpenApi, Body, Endpoint, Method, Request, Response, StatusCode};

use http_kit::error::BoxHttpError;
use http_kit::http_error;
use matchit::Match;
use skyzen_core::{
    error_response,
    middleware::{boxed, BoxFuture, BoxMiddleware, Dispatch, Middleware, Next},
    Error, Extractor, RequestBodyLimit, Requirement,
};
use tracing::debug;

/// One registered handler plus the middleware wrapping it.
struct App {
    endpoint_factory: EndpointFactory,
    middleware: Vec<BoxMiddleware>,
}

impl Debug for App {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("middleware", &self.middleware.len())
            .finish_non_exhaustive()
    }
}

impl App {
    /// Run this endpoint's middleware chain and then the endpoint.
    async fn run(&self, request: &mut Request) -> Result<Response, Error> {
        let terminal = FactoryDispatch(&self.endpoint_factory);
        Next::new(&self.middleware, &terminal).run(request).await
    }
}

/// The handlers registered for one path.
#[derive(Debug, Default)]
struct MethodTable {
    entries: Vec<(Method, App)>,
    /// Handler registered with `.any`, answering whatever the exact entries do not.
    any: Option<App>,
}

/// Chain terminal that serves a request from a freshly built endpoint.
struct FactoryDispatch<'a>(&'a EndpointFactory);

impl Dispatch for FactoryDispatch<'_> {
    fn dispatch<'a>(&'a self, request: &'a mut Request) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let mut endpoint = (self.0)();
            endpoint.respond(request).await.map_err(Error::from)
        })
    }
}

/// Chain terminal that performs route matching once the router's layers have run.
struct MatchDispatch<'r>(&'r Router);

impl Dispatch for MatchDispatch<'_> {
    fn dispatch<'a>(&'a self, request: &'a mut Request) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(self.0.dispatch_matched(request))
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
    inner: Arc<matchit::Router<MethodTable>>,
    /// Middleware wrapping the entire dispatch, outermost first.
    layers: Arc<Vec<BoxMiddleware>>,
    fallback: Option<EndpointFactory>,
    method_not_allowed: Option<EndpointFactory>,
    routes: Arc<Vec<(MethodFilter, String)>>,
    already_router_enabled: bool,
    /// Optional alarm handler for Durable Object alarm events.
    pub(crate) alarm_handler: Option<EndpointFactory>,
    #[cfg(feature = "openapi")]
    openapi_entries: Arc<Vec<RouteOpenApiEntry>>,
}

#[allow(clippy::missing_fields_in_debug)] // the endpoint factories have nothing to render
impl Debug for Router {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut debug_struct = f.debug_struct("Router");
        debug_struct
            .field("routes", &self.routes)
            .field("layers", &self.layers.len())
            .field("has_fallback", &self.fallback.is_some())
            .field("has_method_not_allowed", &self.method_not_allowed.is_some())
            .field("already_router_enabled", &self.already_router_enabled)
            .field("has_alarm_handler", &self.alarm_handler.is_some());
        #[cfg(feature = "openapi")]
        {
            debug_struct.field("openapi_entries", &self.openapi_entries.len());
        }
        debug_struct.finish()
    }
}

http_error!(
    /// The error the built-in 404 response is rendered from.
    pub NotFound,
    StatusCode::NOT_FOUND,
    "Route not found."
);

/// The methods registered for the path a `405` was produced for.
///
/// Available to a [`Route::method_not_allowed`](crate::routing::Route::method_not_allowed)
/// handler, which the router runs with this value in the request extensions.
#[derive(Debug, Clone, Default)]
pub struct AllowedMethods(Vec<Method>);

impl AllowedMethods {
    /// The registered methods, in registration order, with `HEAD` synthesized alongside `GET`.
    #[must_use]
    pub fn methods(&self) -> &[Method] {
        &self.0
    }

    /// Render the methods as an `Allow` header value.
    #[must_use]
    pub fn header_value(&self) -> Option<header::HeaderValue> {
        let list = self
            .0
            .iter()
            .map(Method::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        header::HeaderValue::from_str(&list).ok()
    }
}

impl std::ops::Deref for AllowedMethods {
    type Target = [Method];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Extractor for AllowedMethods {
    type Error = Infallible;
    // Reading the set back out of the extensions is a synchronous clone, so the future is ready on
    // creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(Ok(request
            .extensions()
            .get::<Self>()
            .cloned()
            .unwrap_or_default()))
    }
}

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
    Unmatched,
}

impl Router {
    fn lookup(&self, path: &str, method: &Method) -> RouteLookup<'_> {
        let Ok(Match { value, params }) = self.inner.at(path) else {
            return RouteLookup::Unmatched;
        };

        let found = value
            .entries
            .iter()
            .find(|(app_method, ..)| app_method == method)
            .map(|(.., app)| (app, false))
            .or_else(|| {
                // A HEAD request without a dedicated handler falls back to the
                // GET handler; the response body is discarded after it runs.
                if *method == Method::HEAD {
                    value
                        .entries
                        .iter()
                        .find(|(app_method, ..)| *app_method == Method::GET)
                        .map(|(.., app)| (app, true))
                } else {
                    None
                }
            })
            .or_else(|| value.any.as_ref().map(|app| (app, false)));

        found.map_or_else(
            || RouteLookup::MethodNotAllowed(allowed_methods(&value.entries)),
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

    /// Match the request and run the endpoint it selects, or the fallback that stands in for it.
    ///
    /// Router layers have already run by the time this is reached, which is what lets a CORS or
    /// tracing layer see the 404 and 405 responses produced here.
    async fn dispatch_matched(&self, request: &mut Request) -> Result<Response, Error> {
        let method = request.method().clone();
        match self.lookup(request.uri().path(), &method) {
            RouteLookup::Endpoint {
                app,
                head_fallback,
                params,
            } => {
                request.extensions_mut().insert(Params::new(params));
                let mut response = app.run(request).await?;
                if head_fallback {
                    *response.body_mut() = Body::empty();
                }
                Ok(response)
            }
            RouteLookup::MethodNotAllowed(allow) => {
                let allow = AllowedMethods(allow);
                match &self.method_not_allowed {
                    Some(factory) => {
                        request.extensions_mut().insert(allow);
                        run_endpoint(factory, request).await
                    }
                    None => Ok(method_not_allowed_response(&allow)),
                }
            }
            RouteLookup::Unmatched => match &self.fallback {
                Some(factory) => run_endpoint(factory, request).await,
                None => Ok(error_response(&NotFound::new())),
            },
        }
    }

    async fn call(&self, request: &mut Request) -> Result<Response, Error> {
        debug!(
            method = request.method().as_str(),
            path = request.uri().path(),
            "request received"
        );

        if self.already_router_enabled {
            request.extensions_mut().insert(self.clone());
        }
        // Insert-if-absent: a router mounted as an endpoint inside another router must not clear
        // the outer router's `BodyLimit`.
        if request.extensions().get::<RequestBodyLimit>().is_none() {
            request.extensions_mut().insert(RequestBodyLimit::default());
        }

        let terminal = MatchDispatch(self);
        Next::new(&self.layers, &terminal).run(request).await
    }

    /// Dispatch the provided [`Request`] through the router and return the produced [`Response`].
    ///
    /// Unmatched paths and unregistered methods produce an ordinary `Ok` response carrying the
    /// 404 or 405 status, so a caller inspects `response.status()` rather than the error.
    ///
    /// # Errors
    ///
    /// Returns any error bubbled up by the matched endpoint, such as rejections from middleware.
    ///
    /// Cloning a router is cheap, so prefer `router.clone().go(request)` when invoking it from
    /// tests or asynchronous workers.
    pub async fn go(&self, mut request: Request) -> Result<Response, BoxHttpError> {
        self.call(&mut request)
            .await
            .map_err(Error::into_boxed_http_error)
    }

    /// Wrap the whole router in `middleware`, including its 404 and 405 responses.
    ///
    /// Each call wraps what is already there, so the most recent call is the outermost layer and
    /// everything registered with [`Route::layer`](crate::routing::Route::layer) stays innermost.
    /// Prefer `Route::layer` where you can: only layers known before the build take part in the
    /// wiring validation [`Route::try_build`](crate::routing::Route::try_build) performs.
    #[must_use]
    pub fn layer<M: Middleware>(mut self, middleware: M) -> Self {
        Arc::make_mut(&mut self.layers).insert(0, boxed(middleware));
        self
    }

    /// Every `(method, path)` pair the router answers, for introspection and tests.
    #[must_use]
    pub fn routes(&self) -> &[(MethodFilter, String)] {
        &self.routes
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
        #[cfg(feature = "openapi")]
        {
            OpenApi::from_entries(&self.openapi_entries)
        }

        #[cfg(not(feature = "openapi"))]
        {
            OpenApi::default()
        }
    }

    /// The `OpenAPI` entries this router was built from, for re-export by a router that mounts it.
    #[cfg(feature = "openapi")]
    pub(crate) fn openapi_entries(&self) -> &[RouteOpenApiEntry] {
        &self.openapi_entries
    }

    /// Create a fresh alarm endpoint, if one was registered via [`Route::on_alarm`].
    #[must_use]
    pub fn alarm_endpoint(&self) -> Option<BoxEndpoint> {
        self.alarm_handler.as_ref().map(|factory| factory())
    }
}

/// Build and run a stored endpoint once.
async fn run_endpoint(factory: &EndpointFactory, request: &mut Request) -> Result<Response, Error> {
    let mut endpoint = factory();
    endpoint.respond(request).await.map_err(Error::from)
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
fn method_not_allowed_response(allow: &AllowedMethods) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    if let Some(value) = allow.header_value() {
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
    // Reading the router back out of the extensions is a synchronous clone, so the future is ready
    // on creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or(RouterNotExist::new()),
        )
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
    /// More than one catch-all handler was registered for the same path.
    RepeatedAny {
        /// Path that already has a catch-all handler.
        path: String,
    },
    /// The assembled path could never match a request.
    InvalidPath {
        /// The path as the route tree assembled it.
        path: String,
    },
    /// A router was mounted under a prefix containing a pattern segment.
    NestedPatternPrefix {
        /// The mount prefix that cannot be stripped.
        path: String,
    },
    /// An endpoint needs a value that no middleware on its ancestor chain provides.
    MissingProvision {
        /// Path of the endpoint whose wiring is incomplete.
        path: String,
        /// Which requests that endpoint answers.
        method: MethodFilter,
        /// The value that is missing.
        requirement: Requirement,
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
            Self::RepeatedAny { path } => write!(
                f,
                "path `{path}` registers more than one catch-all (`any`) handler"
            ),
            Self::InvalidPath { path } => write!(
                f,
                "route path `{path}` cannot match any request: every mount prefix must start \
                 with `/`"
            ),
            Self::NestedPatternPrefix { path } => write!(
                f,
                "a router cannot be mounted at `{path}`: the prefix is removed from the request \
                 path literally, so it must not contain a `{{name}}` segment"
            ),
            Self::MissingProvision {
                path,
                method,
                requirement,
            } => write!(
                f,
                "`{method} {path}` extracts `{}`, but nothing on its route provides it; add {} to \
                 the route, or to the router with `.layer(..)`",
                requirement.description(),
                requirement.hint()
            ),
            Self::MatchitError(error) => write!(f, "invalid route pattern: {error}"),
        }
    }
}

impl std::error::Error for RouteBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MatchitError(error) => Some(error),
            _ => None,
        }
    }
}

impl From<matchit::InsertError> for RouteBuildError {
    fn from(error: matchit::InsertError) -> Self {
        Self::MatchitError(error)
    }
}

/// One endpoint, flattened out of the tree with its full path and ancestor middleware.
struct FlatEndpoint {
    method: MethodFilter,
    factory: EndpointFactory,
    middleware: Vec<BoxMiddleware>,
    requirements: Vec<Requirement>,
}

type FlattenBuf = HashMap<String, Vec<FlatEndpoint>>;

fn flatten(
    path_prefix: &str,
    nodes: Vec<RouteNode>,
    buf: &mut FlattenBuf,
    #[cfg(feature = "openapi")] openapi_entries: &mut Vec<RouteOpenApiEntry>,
) -> Result<(), RouteBuildError> {
    for node in nodes {
        let path = join_path(path_prefix, &node.path);

        match node.node_type {
            RouteNodeType::Route(route) => {
                flatten(
                    &path,
                    route.into_mounted_nodes(),
                    buf,
                    #[cfg(feature = "openapi")]
                    openapi_entries,
                )?;
            }
            RouteNodeType::Endpoint {
                endpoint_factory,
                method,
                openapi,
                requirements,
                middleware,
            } => {
                if !path.starts_with('/') {
                    return Err(RouteBuildError::InvalidPath { path });
                }

                #[cfg(feature = "openapi")]
                if let (Some(openapi), MethodFilter::Exact(exact)) = (openapi, &method) {
                    openapi_entries.push(RouteOpenApiEntry::new(
                        path.clone(),
                        exact.clone(),
                        openapi,
                    ));
                }
                #[cfg(not(feature = "openapi"))]
                let _ = openapi;

                buf.entry(path).or_default().push(FlatEndpoint {
                    method,
                    factory: endpoint_factory,
                    middleware,
                    requirements,
                });
            }
            RouteNodeType::Nested { router, middleware } => {
                if !path.starts_with('/') {
                    return Err(RouteBuildError::InvalidPath { path });
                }
                // The prefix is removed from the raw request path by string comparison, so a
                // pattern segment in it would leave no stable prefix to remove.
                if path.contains('{') {
                    return Err(RouteBuildError::NestedPatternPrefix { path });
                }

                #[cfg(feature = "openapi")]
                openapi_entries.extend(super::prefixed_openapi_entries(&path, &router));

                let nested = NestedRouter::new(&path, router);
                // The mount point itself and everything beneath it, for every method: the inner
                // router decides which of them it answers, including its own 404 and 405.
                for mounted in [path.clone(), join_path(&path, "/{*skyzen_nested}")] {
                    let endpoint = nested.clone();
                    buf.entry(mounted).or_default().push(FlatEndpoint {
                        method: MethodFilter::Any,
                        factory: Arc::new(move || BoxEndpoint::new(endpoint.clone())),
                        middleware: middleware.clone(),
                        requirements: Vec::new(),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Build a [`Router`] from the provided [`Route`].
///
/// # Errors
///
/// Returns [`RouteBuildError`] if the route tree contains conflicting method registrations, an
/// unusable path, an endpoint whose extractors are not wired, or if the underlying path matcher
/// rejects the route definition.
pub fn build(route: Route) -> Result<Router, RouteBuildError> {
    let Route {
        nodes,
        layers,
        fallback,
        method_not_allowed,
    } = route;

    let mut buf = FlattenBuf::new();
    #[cfg(feature = "openapi")]
    let mut openapi_entries = Vec::new();
    flatten(
        "",
        nodes,
        &mut buf,
        #[cfg(feature = "openapi")]
        &mut openapi_entries,
    )?;

    let global_provisions: HashSet<std::any::TypeId> = layers
        .iter()
        .flat_map(|layer| layer.provisions_dyn())
        .collect();

    for entry in fallback.iter().chain(method_not_allowed.iter()) {
        check_requirements(
            "<fallback>",
            &MethodFilter::Any,
            entry.requirements.iter(),
            &global_provisions,
        )?;
    }

    let mut matcher = matchit::Router::new();
    let mut routes = Vec::new();
    for (path, endpoints) in buf {
        let table = build_method_table(&path, endpoints, &global_provisions, &mut routes)?;
        matcher.insert(path, table)?;
    }
    routes.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
    });

    Ok(Router {
        inner: Arc::new(matcher),
        layers: Arc::new(layers),
        fallback: fallback.map(|entry| entry.factory),
        method_not_allowed: method_not_allowed.map(|entry| entry.factory),
        routes: Arc::new(routes),
        already_router_enabled: false,
        alarm_handler: None,
        #[cfg(feature = "openapi")]
        openapi_entries: Arc::new(openapi_entries),
    })
}

/// Assemble one path's handlers, rejecting duplicate registrations and unwired extractors.
fn build_method_table(
    path: &str,
    endpoints: Vec<FlatEndpoint>,
    global_provisions: &HashSet<std::any::TypeId>,
    routes: &mut Vec<(MethodFilter, String)>,
) -> Result<MethodTable, RouteBuildError> {
    let mut table = MethodTable::default();
    let mut seen = HashSet::new();

    for endpoint in endpoints {
        let mut provisions = global_provisions.clone();
        provisions.extend(
            endpoint
                .middleware
                .iter()
                .flat_map(|middleware| middleware.provisions_dyn()),
        );
        check_requirements(
            path,
            &endpoint.method,
            endpoint.requirements.iter(),
            &provisions,
        )?;

        let app = App {
            endpoint_factory: endpoint.factory,
            middleware: endpoint.middleware,
        };

        routes.push((endpoint.method.clone(), path.to_owned()));
        match endpoint.method {
            MethodFilter::Exact(method) => {
                if !seen.insert(method.clone()) {
                    return Err(RouteBuildError::RepeatedMethod {
                        path: path.to_owned(),
                        method,
                    });
                }
                table.entries.push((method, app));
            }
            MethodFilter::Any => {
                if table.any.replace(app).is_some() {
                    return Err(RouteBuildError::RepeatedAny {
                        path: path.to_owned(),
                    });
                }
            }
        }
    }

    Ok(table)
}

fn check_requirements<'a>(
    path: &str,
    method: &MethodFilter,
    requirements: impl Iterator<Item = &'a Requirement>,
    provisions: &HashSet<std::any::TypeId>,
) -> Result<(), RouteBuildError> {
    for requirement in requirements {
        if !provisions.contains(&requirement.type_id()) {
            return Err(RouteBuildError::MissingProvision {
                path: path.to_owned(),
                method: method.clone(),
                requirement: *requirement,
            });
        }
    }
    Ok(())
}

impl Endpoint for Router {
    type Error = BoxHttpError;
    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        // Propagate errors to let middleware or runtime handle them
        self.call(request)
            .await
            .map_err(Error::into_boxed_http_error)
    }
}

#[cfg(test)]
mod tests {
    use super::{build, MethodFilter, RouteBuildError};
    use crate::{
        header,
        middleware::{from_fn, ErrorHandlingMiddleware, Middleware, Next},
        routing::{AllowedMethods, CreateRouteNode, Params, Route},
        utils::{Form, Json},
        Body, Error, Method, Request, Response, Result, StatusCode, ToSchema,
    };
    use serde::{Deserialize, Serialize};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
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

    #[tokio::test]
    async fn collapses_duplicate_separators_between_mount_and_child() {
        let router = build(Route::new((
            "/api/".route(("/v1".at(|| async { Result::Ok("pong") }),)),
        )))
        .unwrap();

        assert_eq!(
            router.routes(),
            [(MethodFilter::Exact(Method::GET), "/api/v1".to_owned())]
        );
        let response = router.clone().go(get_request("/api/v1")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn rejects_a_mount_prefix_without_a_leading_slash() {
        let error = build(Route::new((
            "api".route(("/v1".at(|| async { Result::Ok("pong") }),)),
        )))
        .unwrap_err();

        let RouteBuildError::InvalidPath { path } = &error else {
            panic!("expected an invalid-path error, got {error:?}");
        };
        assert_eq!(path, "api/v1");
        assert!(error.to_string().contains("api/v1"));
    }

    #[test]
    fn routes_lists_every_registration() {
        let router = build(Route::new((
            "/items"
                .at(|| async { Result::Ok("list") })
                .post(|| async { Result::Ok("new") }),
            "/health".any(|| async { Result::Ok("ok") }),
        )))
        .unwrap();

        assert_eq!(
            router.routes(),
            [
                (MethodFilter::Any, "/health".to_owned()),
                (MethodFilter::Exact(Method::GET), "/items".to_owned()),
                (MethodFilter::Exact(Method::POST), "/items".to_owned()),
            ]
        );
    }

    #[derive(Debug, Default)]
    struct HeaderMiddleware;

    impl Middleware for HeaderMiddleware {
        async fn handle(
            &self,
            request: &mut Request,
            next: Next<'_>,
        ) -> std::result::Result<Response, Error> {
            let mut response = next.run(request).await?;
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
    async fn node_middleware_covers_only_its_own_node() {
        let router = build(Route::new((
            "/marked"
                .at(|| async { Result::Ok("yes") })
                .with(HeaderMiddleware),
            "/plain".at(|| async { Result::Ok("no") }),
        )))
        .unwrap();

        let marked = router.clone().go(get_request("/marked")).await.unwrap();
        assert!(marked.headers().contains_key("x-middleware"));

        let plain = router.clone().go(get_request("/plain")).await.unwrap();
        assert!(!plain.headers().contains_key("x-middleware"));
    }

    #[tokio::test]
    async fn middleware_state_is_shared_across_requests() {
        /// A middleware that counts, written the obvious way. Before middleware became
        /// `&self`-shared, each request saw its own clone and this stayed at 1 forever.
        #[derive(Debug, Default)]
        struct CountRequests {
            seen: AtomicUsize,
        }

        impl Middleware for CountRequests {
            async fn handle(
                &self,
                request: &mut Request,
                next: Next<'_>,
            ) -> std::result::Result<Response, Error> {
                let seen = self.seen.fetch_add(1, Ordering::SeqCst) + 1;
                let mut response = next.run(request).await?;
                response.headers_mut().insert(
                    header::HeaderName::from_static("x-seen"),
                    header::HeaderValue::from_str(&seen.to_string()).unwrap(),
                );
                Ok(response)
            }
        }

        let router = build(
            Route::new(("/ping".at(|| async { Result::Ok("pong") }),))
                .with(CountRequests::default()),
        )
        .unwrap();

        for expected in ["1", "2", "3"] {
            let response = router.clone().go(get_request("/ping")).await.unwrap();
            assert_eq!(response.headers().get("x-seen").unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn closure_middleware_shares_its_captured_state() {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let router = build(
            Route::new(("/ping".at(|| async { Result::Ok("pong") }),)).layer(from_fn(
                move |request, next| {
                    let counter = Arc::clone(&counter);
                    Box::pin(async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        next.run(request).await
                    })
                },
            )),
        )
        .unwrap();

        router.clone().go(get_request("/ping")).await.unwrap();
        router.clone().go(get_request("/missing")).await.unwrap();

        // The layer also saw the request that matched no route.
        assert_eq!(seen.load(Ordering::SeqCst), 2);
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

    #[test]
    fn prevents_duplicate_catch_all_handlers() {
        let error = build(Route::new((
            "/dup".any(|| async { Result::Ok("first") }),
            "/dup".any(|| async { Result::Ok("second") }),
        )))
        .unwrap_err();
        assert!(matches!(error, RouteBuildError::RepeatedAny { path } if path == "/dup"));
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
            "/items".on(Method::CONNECT, || async { Result::Ok("connect") }),
        )))
        .unwrap();

        for (method, expected) in [
            (Method::PATCH, "patched"),
            (Method::HEAD, ""),
            (Method::OPTIONS, "options"),
            (Method::TRACE, "trace"),
            (Method::CONNECT, "connect"),
        ] {
            let request = request_with_method("/items", method);
            let response = router.clone().go(request).await.unwrap();
            let body = response.into_body().into_string().await.unwrap();
            assert_eq!(body, expected);
        }
    }

    #[tokio::test]
    async fn any_answers_every_method_but_yields_to_exact_registrations() {
        let router = build(Route::new((
            "/thing".any(|| async { Result::Ok("any") }),
            "/thing".post(|| async { Result::Ok("post") }),
        )))
        .unwrap();

        for (method, expected) in [
            (Method::GET, "any"),
            (Method::DELETE, "any"),
            (Method::POST, "post"),
        ] {
            let request = request_with_method("/thing", method);
            let response = router.clone().go(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
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
        let body = response.into_body().into_string().await.unwrap();
        assert!(
            body.contains("@scalar/api-reference"),
            "enable_api_doc should serve Scalar HTML"
        );
        assert!(body.contains("id=\"api-reference\""));
    }

    #[tokio::test]
    async fn redoc_route_serves_redoc_html() {
        async fn ping() -> Result<&'static str> {
            Ok("pong")
        }

        let route = Route::new(("/ping".at(ping),));
        let docs = route.openapi().redoc_route("/redoc");
        let router = build(Route::new((route, docs))).unwrap();

        let response = router.clone().go(get_request("/redoc")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().into_string().await.unwrap();
        assert!(
            body.contains("redoc"),
            "redoc_route should serve Redoc HTML"
        );
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn websocket_routes_require_upgrades() {
        use crate::header::{self, HeaderValue};
        use http_kit::HttpError;

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
        let response = router.clone().go(get_request("/unknown")).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, r#"{"error":"Route not found."}"#);
    }

    #[tokio::test]
    async fn a_fallback_handler_replaces_the_built_in_404() {
        let router = build(
            Route::new(("/known".at(|| async { Result::Ok("here") }),)).fallback(
                |uri: crate::Uri| async move { Result::Ok(format!("no such page: {}", uri.path())) },
            ),
        )
        .unwrap();

        let response = router.clone().go(get_request("/nowhere")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "no such page: /nowhere");
    }

    #[tokio::test]
    async fn a_method_not_allowed_handler_can_read_the_registered_methods() {
        async fn rejected(allowed: AllowedMethods) -> Result<String> {
            Ok(allowed
                .methods()
                .iter()
                .map(Method::as_str)
                .collect::<Vec<_>>()
                .join("|"))
        }

        let router = build(
            Route::new(("/items".at(|| async { Result::Ok("list") }),))
                .method_not_allowed(rejected),
        )
        .unwrap();

        let response = router
            .clone()
            .go(request_with_method("/items", Method::DELETE))
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "GET|HEAD");
    }

    #[tokio::test]
    async fn a_router_layer_wraps_the_not_found_path() {
        let router = build(
            Route::new(("/known".at(|| async { Result::Ok("here") }),)).layer(HeaderMiddleware),
        )
        .unwrap();

        let response = router.clone().go(get_request("/nowhere")).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().contains_key("x-middleware"));
    }

    #[tokio::test]
    async fn a_post_build_router_layer_also_wraps_everything() {
        let router = build(Route::new(("/known".at(|| async { Result::Ok("here") }),)))
            .unwrap()
            .layer(HeaderMiddleware);

        let response = router
            .clone()
            .go(request_with_method("/known", Method::DELETE))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(response.headers().contains_key("x-middleware"));
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

    #[tokio::test]
    async fn a_mounted_router_sees_paths_without_its_prefix() {
        async fn list(uri: crate::Uri) -> Result<String> {
            Ok(format!("inner saw {}", uri.path()))
        }

        let inner = build(Route::new((
            "/users".at(list),
            "/".at(|| async { Result::Ok("inner root") }),
        )))
        .unwrap();
        let router = build(Route::new((
            "/api".nest(inner),
            "/health".at(|| async { Result::Ok("ok") }),
        )))
        .unwrap();

        let response = router.clone().go(get_request("/api/users")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "inner saw /users");

        // The mount point itself reaches the inner router's root.
        let response = router.clone().go(get_request("/api")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "inner root");

        // Sibling routes are untouched.
        let response = router.clone().go(get_request("/health")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn a_mounted_router_keeps_its_own_fallback_and_405() {
        let inner = build(
            Route::new(("/users".at(|| async { Result::Ok("users") }),))
                .fallback(|| async { Result::Ok("inner fallback") }),
        )
        .unwrap();
        let router = build(
            Route::new(("/api".nest(inner),)).fallback(|| async { Result::Ok("outer fallback") }),
        )
        .unwrap();

        // Unknown path *under* the prefix: the inner router answers.
        let response = router.clone().go(get_request("/api/nope")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "inner fallback");

        // Unknown path outside it: the outer router answers.
        let response = router.clone().go(get_request("/nope")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "outer fallback");

        // The inner router still decides which methods it answers.
        let response = router
            .clone()
            .go(request_with_method("/api/users", Method::DELETE))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn a_mounted_router_keeps_the_query_string() {
        async fn echo(
            query: crate::extract::Query<std::collections::BTreeMap<String, String>>,
            uri: crate::Uri,
        ) -> Result<String> {
            let seen: Vec<String> = query
                .0
                .iter()
                .map(|(key, value)| [key.as_str(), value.as_str()].join("="))
                .collect();
            Ok([uri.path(), &seen.join(",")].join(" "))
        }

        let inner = build(Route::new(("/search".at(echo),))).unwrap();
        let router = build(Route::new(("/api".nest(inner),))).unwrap();

        let response = router
            .clone()
            .go(get_request("/api/search?q=rust"))
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "/search q=rust");
    }

    #[tokio::test]
    async fn a_router_mounted_at_the_root_or_a_trailing_slash_still_routes() {
        async fn echo(uri: crate::Uri) -> Result<String> {
            Ok(uri.path().to_owned())
        }

        for prefix in ["/", "/api/"] {
            let inner = build(Route::new(("/users".at(echo),))).unwrap();
            let router = build(Route::new((prefix.nest(inner),))).unwrap();

            let path = if prefix == "/" {
                "/users"
            } else {
                "/api/users"
            };
            let response = router.clone().go(get_request(path)).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "a router mounted at `{prefix}` should answer `{path}`"
            );
            let body = response.into_body().into_string().await.unwrap();
            assert_eq!(body, "/users", "mounted at `{prefix}`");
        }
    }

    #[test]
    fn a_router_cannot_be_mounted_under_a_pattern_prefix() {
        let inner = build(Route::new(("/x".at(|| async { Result::Ok("x") }),))).unwrap();
        let error = build(Route::new(("/tenants/{tenant}".nest(inner),))).unwrap_err();
        assert!(matches!(error, RouteBuildError::NestedPatternPrefix { .. }));
    }

    #[test]
    fn a_mounted_router_re_exports_its_operations_under_the_prefix() {
        let inner = build(Route::new(("/widgets".post(create_widget),))).unwrap();
        let openapi = Route::new(("/api".nest(inner),)).build().openapi();

        assert!(openapi
            .operations()
            .iter()
            .any(|operation| operation.path == "/api/widgets" && operation.method == Method::POST));
    }

    #[derive(Debug, Deserialize, ToSchema)]
    struct ItemPath {
        id: u64,
    }

    #[skyzen::openapi]
    async fn show_item(
        crate::extract::Path(path): crate::extract::Path<ItemPath>,
    ) -> Result<String> {
        Ok(path.id.to_string())
    }

    #[test]
    fn a_path_extractor_types_the_route_pattern_parameter_exactly_once() {
        let spec = Route::new(("/items/{id}".at(show_item),))
            .openapi()
            .to_utoipa_spec();
        let json = serde_json::to_value(&spec).expect("serialize spec");
        let parameters = json["paths"]["/items/{id}"]["get"]["parameters"]
            .as_array()
            .expect("operation should declare parameters");

        let described: Vec<_> = parameters
            .iter()
            .filter(|parameter| parameter["name"] == "id")
            .collect();
        assert_eq!(
            described.len(),
            1,
            "the route pattern and the extractor describe the same parameter"
        );
        assert_eq!(described[0]["in"], "path");
        assert_eq!(described[0]["required"], true);
        assert_eq!(
            described[0]["schema"]["type"], "integer",
            "the extractor's type must replace the untyped string default"
        );
    }

    #[tokio::test]
    async fn a_path_extractor_deserializes_the_captured_segments() {
        async fn show(path: crate::extract::Path<(String, u32)>) -> Result<String> {
            let crate::extract::Path((user, post)) = path;
            Ok(format!("{user}/{post}"))
        }

        let router = build(Route::new(("/users/{user}/posts/{post}".at(show),))).unwrap();
        let response = router
            .clone()
            .go(get_request("/users/ada/posts/17"))
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "ada/17");

        let response = router
            .clone()
            .go(get_request("/users/ada/posts/seventeen"))
            .await
            .unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
