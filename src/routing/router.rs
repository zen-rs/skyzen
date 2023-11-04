use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
};

use super::{BoxEndpoint, Params, Route, RouteNode, RouteNodeType, SharedMiddleware};
use crate::{
    async_trait, middleware::Next, Endpoint, Error, Method, Request, Response, StatusCode,
};

use matchit::Match;

// The entrance of request,composing of endpoint and middlewares.
#[allow(missing_debug_implementations)]
pub struct App {
    endpoint: BoxEndpoint,
    middlewares: Vec<SharedMiddleware>,
}

pub struct Router {
    inner: matchit::Router<Vec<(Method, App)>>,
    middlewares: Vec<SharedMiddleware>,
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
    pub fn search<'app, 'path, 'temp>(
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
            let next = Next::new(&self.middlewares, &NotFoundEndpoint);
            next.run(request).await
        }
    }
}

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
fn flatten(
    path_prefix: &str,
    route: Vec<RouteNode>,
    buf: &mut FlattenBuf,
    middlewares: Vec<SharedMiddleware>,
) {
    for node in route {
        let path = format!("{}{}", path_prefix, node.path);

        match node.node_type {
            RouteNodeType::Route(route) => {
                flatten(
                    &path,
                    route,
                    buf,
                    [middlewares.clone(), node.middlewares].concat(),
                );
            }
            RouteNodeType::Endpoint { endpoint, method } => {
                let entry = buf.entry(path).or_insert(Vec::new());

                entry.push((
                    method,
                    App {
                        endpoint,
                        middlewares: middlewares.clone(),
                    },
                ))
            }
        }
    }
}

pub fn build(route: Route) -> Result<Router, RouteBuildError> {
    let mut buf = HashMap::new();
    flatten("", route.nodes, &mut buf, route.middlewares.clone());
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
        inner: router,
        middlewares: route.middlewares,
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
