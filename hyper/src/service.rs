use futures_util::{stream::MapOk, TryStreamExt};
use http_body_util::{BodyDataStream, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    service::Service,
};

use skyzen::{runtime::native::Spawner, utils::Bytes, BodyError, Endpoint};
use std::{future::Future, pin::Pin};

type BoxFuture<T> = Pin<Box<dyn 'static + Send + Future<Output = T>>>;

/// Hyper service adapter for skyzen endpoints.
#[derive(Debug)]
pub struct IntoService<E> {
    endpoint: E,
    spawner: Spawner,
}

impl<E: Endpoint + Clone> IntoService<E> {
    /// Create a new service with the given endpoint and spawner.
    pub const fn new(endpoint: E, spawner: Spawner) -> Self {
        Self { endpoint, spawner }
    }
}

impl<E: Endpoint + Send + Sync + Clone + 'static> Service<hyper::Request<Incoming>>
    for IntoService<E>
{
    type Response = hyper::Response<StreamBody<MapOk<skyzen::Body, fn(Bytes) -> Frame<Bytes>>>>;
    type Error = E::Error;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn call(&self, mut req: hyper::Request<Incoming>) -> Self::Future {
        // TODO: Rewrite when impl Trait in associated types stablized
        let mut endpoint = self.endpoint.clone();
        let spawner = self.spawner.clone();
        let fut = async move {
            let on_upgrade = hyper::upgrade::on(&mut req);
            let mut request: skyzen::Request =
                skyzen::Request::from(req.map(BodyDataStream::new).map(|body| {
                    skyzen::Body::from_stream(
                        body.map_err(|error| BodyError::Other(Box::new(error))),
                    )
                }));
            request.extensions_mut().insert(on_upgrade);
            request.extensions_mut().insert(spawner);
            let response: Result<skyzen::Response, _> = endpoint.respond(&mut request).await;

            let response: Result<hyper::Response<skyzen::Body>, _> = response;

            response.map(|response| {
                response.map(|body| {
                    let body: MapOk<skyzen::Body, fn(Bytes) -> Frame<Bytes>> =
                        body.map_ok(Frame::data);

                    StreamBody::new(body)
                })
            })
        };

        Box::pin(fut)
    }
}
