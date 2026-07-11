use executor_core::AnyExecutor;
use futures_util::{stream::MapOk, TryStreamExt};
use http_body_util::{BodyDataStream, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    service::Service,
};
use std::{convert::Infallible, future::Future, net::SocketAddr, pin::Pin, sync::Arc};

use skyzen_core::{error_response, BodyError, Endpoint, HttpError, PeerAddr};

type BoxFuture<T> = Pin<Box<dyn 'static + Send + Future<Output = T>>>;
type Bytes = http_kit::utils::Bytes;

/// Hyper service adapter for skyzen endpoints.
///
/// This is the canonical adapter for embedding Skyzen into a hyper server. It injects the peer
/// address (when provided via [`IntoService::with_peer_addr`]) so the `PeerAddr`/`ClientIp`
/// extractors work, and converts handler errors into JSON HTTP responses (hiding 5xx detail)
/// instead of tearing down the connection.
#[derive(Debug)]
pub struct IntoService<E> {
    endpoint: E,
    executor: Arc<AnyExecutor>,
    peer_addr: Option<SocketAddr>,
}

impl<E: Endpoint + Clone> IntoService<E> {
    /// Create a new service with the given endpoint and executor.
    pub const fn new(endpoint: E, executor: Arc<AnyExecutor>) -> Self {
        Self {
            endpoint,
            executor,
            peer_addr: None,
        }
    }

    /// Attach the remote peer address of the connection this service handles.
    ///
    /// Provide it from your accept loop so the `PeerAddr` and `ClientIp` extractors resolve.
    #[must_use]
    pub const fn with_peer_addr(mut self, peer_addr: SocketAddr) -> Self {
        self.peer_addr = Some(peer_addr);
        self
    }
}

impl<E: Endpoint + Send + Sync + Clone + 'static> Service<hyper::Request<Incoming>>
    for IntoService<E>
{
    type Response =
        hyper::Response<StreamBody<MapOk<skyzen_core::Body, fn(Bytes) -> Frame<Bytes>>>>;
    // Handler errors are converted into HTTP responses, so the service itself never fails.
    type Error = Infallible;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn call(&self, mut req: hyper::Request<Incoming>) -> Self::Future {
        // TODO: Rewrite when impl Trait in associated types stabilized
        let mut endpoint = self.endpoint.clone();
        let executor = self.executor.clone();
        let peer_addr = self.peer_addr;
        let fut = async move {
            let on_upgrade = hyper::upgrade::on(&mut req);
            let method = req.method().clone();
            let path = req.uri().path().to_owned();
            let mut request: skyzen_core::Request =
                skyzen_core::Request::from(req.map(BodyDataStream::new).map(|body| {
                    skyzen_core::Body::from_stream(
                        body.map_err(|error| BodyError::Other(Box::new(error))),
                    )
                }));
            request.extensions_mut().insert(on_upgrade);
            request.extensions_mut().insert(executor);
            if let Some(peer_addr) = peer_addr {
                request.extensions_mut().insert(PeerAddr::new(peer_addr));
            }

            let response: skyzen_core::Response = match endpoint.respond(&mut request).await {
                Ok(response) => {
                    tracing::info!(
                        method = method.as_str(),
                        path = path.as_str(),
                        status = response.status().as_u16(),
                        "request completed"
                    );
                    response
                }
                Err(error) => {
                    let status = error.status();
                    if status.is_server_error() {
                        tracing::error!(
                            method = method.as_str(),
                            path = path.as_str(),
                            status = status.as_u16(),
                            error = %error,
                            "internal server error"
                        );
                    } else {
                        tracing::warn!(
                            method = method.as_str(),
                            path = path.as_str(),
                            status = status.as_u16(),
                            error = %error,
                            "client error"
                        );
                    }
                    error_response(&error)
                }
            };

            Ok(response.map(|body| {
                let body: MapOk<skyzen_core::Body, fn(Bytes) -> Frame<Bytes>> =
                    body.map_ok(Frame::data);

                StreamBody::new(body)
            }))
        };

        Box::pin(fut)
    }
}
