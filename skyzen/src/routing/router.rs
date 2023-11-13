use std::{
    collections::{HashMap, HashSet},
    fmt::{Debug, Formatter},
    ops::Deref,
    sync::Arc,
};

use super::{BoxEndpoint, Params, Route, RouteNode, RouteNodeType, SharedMiddleware};
use crate::{
    async_trait, middleware::Next, Endpoint, Error, Method, Request, Response, StatusCode,
};

use matchit::Match;
use skyzen_core::Extractor;
use smallvec::SmallVec;

// The entrance of request,composing of endpoint and middlewares.
pub struct App {
    endpoint: BoxEndpoint,
    middlewares: SmallVec<[SharedMiddleware; 5]>,
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

#[async_trait]
impl Endpoint for NotFoundEndpoint {
    async fn call_endpoint(&self, _request: &mut Request) -> crate::Result<Response> {
        Err(Error::new(RouteNotFound, StatusCode::NOT_FOUND))
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
            request.insert_extension(self.clone());
        }

        let path = request.uri().path();
        let method = request.method();

        if let Some(Match { value, params }) = self.search(path, method) {
            let next = Next::new(&value.middlewares, value.endpoint.deref());
            let params: Vec<(String, String)> = params
                .iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            let params = Params::new(params);
            request.insert_extension(params);
            next.run(request).await
        } else {
            let next = Next::new(&[], &NotFoundEndpoint);
            next.run(request).await
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

#[async_trait]
impl Extractor for Router {
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        let router = request
            .get_extension()
            .map(|v: &Router| v.clone())
            .ok_or(RouterNotExist)?;
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
                middlewares,
            } => {
                let entry = buf.entry(path).or_insert(Vec::new());

                entry.push((
                    method,
                    App {
                        endpoint,
                        middlewares: middlewares.into(),
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

#[async_trait]
impl Endpoint for Router {
    async fn call_endpoint(&self, request: &mut Request) -> crate::Result<Response> {
        log::info!(method = request.method().as_str(),path=request.uri().path() ;"Request Received");
        Ok(self.call(request).await.unwrap_or_else(|error| {
            let mut response=Response::empty();
            response.set_status(error.status());
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
