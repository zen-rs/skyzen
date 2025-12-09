use executor_core::AnyExecutor;
use futures_util::{stream::MapOk, TryStreamExt};
use http_body_util::{BodyDataStream, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    service::Service,
};
use std::{future::Future, pin::Pin, sync::Arc};

use skyzen_core::{BodyError, Endpoint};

type BoxFuture<T> = Pin<Box<dyn 'static + Send + Future<Output = T>>>;
type Bytes = http_kit::utils::Bytes;

/// Hyper service adapter for skyzen endpoints.
#[derive(Debug)]
pub struct IntoService<E> {
    endpoint: E,
    executor: Arc<AnyExecutor>,
}

impl<E: Endpoint + Clone> IntoService<E> {
    /// Create a new service with the given endpoint and executor.
    pub const fn new(endpoint: E, executor: Arc<AnyExecutor>) -> Self {
        Self { endpoint, executor }
    }
}

impl<E: Endpoint + Send + Sync + Clone + 'static> Service<hyper::Request<Incoming>>
    for IntoService<E>
{
    type Response =
        hyper::Response<StreamBody<MapOk<skyzen_core::Body, fn(Bytes) -> Frame<Bytes>>>>;
    type Error = E::Error;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn call(&self, mut req: hyper::Request<Incoming>) -> Self::Future {
        // TODO: Rewrite when impl Trait in associated types stabilized
        let mut endpoint = self.endpoint.clone();
        let executor = self.executor.clone();
        let fut = async move {
            let on_upgrade = hyper::upgrade::on(&mut req);
            let mut request: skyzen_core::Request =
                skyzen_core::Request::from(req.map(BodyDataStream::new).map(|body| {
                    skyzen_core::Body::from_stream(
                        body.map_err(|error| BodyError::Other(Box::new(error))),
                    )
                }));
            request.extensions_mut().insert(on_upgrade);
            request.extensions_mut().insert(executor);
            let response: Result<skyzen_core::Response, _> = endpoint.respond(&mut request).await;

            let response: Result<hyper::Response<skyzen_core::Body>, _> = response;

            response.map(|response| {
                response.map(|body| {
                    let body: MapOk<skyzen_core::Body, fn(Bytes) -> Frame<Bytes>> =
                        body.map_ok(Frame::data);

                    StreamBody::new(body)
                })
            })
        };

        Box::pin(fut)
    }
}
