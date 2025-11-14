use std::{
    collections::{HashMap, HashSet},
    fmt::{Debug, Formatter},
    sync::Arc,
};
use tokio::sync::Mutex;

use super::{BoxEndpoint, Params, Route, RouteNode, RouteNodeType};
use crate::{Endpoint, Method, Request, Response, StatusCode};

use matchit::Match;
use skyzen_core::Extractor;

// The entrance of request,composing of endpoint
pub struct App {
    endpoint: Arc<Mutex<BoxEndpoint>>,
    // middlewares: SmallVec<[SharedMiddleware; 5]>, // Simplified for now
}

/// An HTTP router.
/// `Router` uses `Arc` internally, so it can safely be shared across threads.
#[derive(Clone)]
pub struct Router {
    inner: Arc<matchit::Router<Vec<(Method, App)>>>,
    programable_router: bool,
}

impl Debug for Router {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("programable_router", &self.programable_router)
            .finish()
    }
}

struct NotFoundEndpoint;

impl Endpoint for NotFoundEndpoint {
    async fn respond(&mut self, _request: &mut Request) -> http_kit::Result<Response> {
        Err(http_kit::Error::msg("Route not found").set_status(StatusCode::NOT_FOUND))
    }
}

impl_error!(RouteNotFound, "Route not found", "No route is matched.");

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

    async fn call(&self, request: &mut Request) -> crate::Result<Response> {
        if self.programable_router {
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

            // Use tokio's Mutex which is Send-safe
            let endpoint = Arc::clone(&value.endpoint);
            let mut endpoint_guard = endpoint.lock().await;
            endpoint_guard.respond(request).await
        } else {
            let mut not_found = NotFoundEndpoint;
            not_found.respond(request).await
        }
    }

    /// Navigate to a route programmablly.
    pub async fn go(&self, mut request: Request) -> crate::Result<Response> {
        self.call(&mut request).await
    }

    /// Enable programable router, so that you can extract it from request.
    pub fn enable_programable_router(mut self) -> Self {
        self.programable_router = true;
        self
    }
}

impl Extractor for Router {
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        let router = request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(http_kit::Error::msg("Router not found"))?;
        Ok(router)
    }
}

impl_error!(
    RouterNotExist,
    "This programmable router does not exist. Please check whether you have enabled the programmable router.",
    "Error occurs if cannot extract router."
);

#[derive(Debug)]
#[non_exhaustive]
pub enum RouteBuildError {
    RepeatedMethod { path: String, method: Method },
    MatchitError(matchit::InsertError),
}

impl From<matchit::InsertError> for RouteBuildError {
    fn from(error: matchit::InsertError) -> Self {
        Self::MatchitError(error)
    }
}

type FlattenBuf = HashMap<String, Vec<(Method, App)>>;
fn flatten(path_prefix: &str, route: Vec<RouteNode>, buf: &mut FlattenBuf) {
    for node in route {
        let path = format!("{}{}", path_prefix, node.path);

        match node.node_type {
            RouteNodeType::Route(route) => {
                flatten(&path, route.nodes, buf);
            }
            RouteNodeType::Endpoint {
                endpoint,
                method,
                // middlewares, // Disabled for now
            } => {
                let entry = buf.entry(path).or_default();

                entry.push((
                    method,
                    App {
                        endpoint: Arc::new(Mutex::new(endpoint)),
                        // middlewares: middlewares.into(), // Simplified for now
                    },
                ))
            }
        }
    }
}

pub fn build(route: Route) -> Result<Router, RouteBuildError> {
    let mut buf = HashMap::new();
    flatten("", route.nodes, &mut buf);
    let mut router = matchit::Router::new();
    for (path, value) in buf {
        let mut set = HashSet::new();
        for (method, ..) in value.iter() {
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
        programable_router: false,
    })
}

impl Endpoint for Router {
    async fn respond(&mut self, request: &mut Request) -> http_kit::Result<Response> {
        log::info!(method = request.method().as_str(),path=request.uri().path() ;"Request Received");
        Ok(self.call(request).await.unwrap_or_else(|error| {
            let mut response = Response::new(http_kit::Body::empty());
            *response.status_mut() = error.status();
            let mut error_name="Error";
            if error.status().is_server_error(){
                error_name="Server Error"
            }

            if error.status().is_client_error(){
                error_name="Client Error"
            }
            log::error!(message = error.to_string().as_str(),status = error.status().as_str(); "{error_name}");
            response
        }))
    }
}
