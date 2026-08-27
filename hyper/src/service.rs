use executor_core::AnyExecutor;
use futures_util::{Stream, TryStreamExt};
use http_body::SizeHint;
use http_body_util::BodyDataStream;
use hyper::{
    body::{Frame, Incoming},
    service::Service,
};
use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use skyzen_core::{error_response, log_endpoint_error, BodyError, Endpoint, PeerAddr};

type BoxFuture<T> = Pin<Box<dyn 'static + Send + Future<Output = T>>>;
type Bytes = http_kit::utils::Bytes;

/// Response body adapter bridging a skyzen [`Body`](skyzen_core::Body) to hyper.
///
/// Unlike a plain stream wrapper, this adapter reports an exact
/// [`size_hint`](http_body::Body::size_hint) whenever the underlying body knows its length, so
/// fixed-size responses are sent with a `Content-Length` header instead of
/// `Transfer-Encoding: chunked`.
#[derive(Debug)]
pub struct SkyzenBody(skyzen_core::Body);

impl SkyzenBody {
    const fn new(body: skyzen_core::Body) -> Self {
        Self(body)
    }
}

impl http_body::Body for SkyzenBody {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.0)
            .poll_next(cx)
            .map(|option| option.map(|result| result.map(Frame::data)))
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_empty() == Some(true)
    }

    fn size_hint(&self) -> SizeHint {
        self.0
            .len()
            .and_then(|len| u64::try_from(len).ok())
            .map_or_else(SizeHint::default, SizeHint::with_exact)
    }
}

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
    type Response = hyper::Response<SkyzenBody>;
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
                    log_endpoint_error(&error, &method, path.as_str());
                    error_response(&error)
                }
            };

            Ok(response.map(SkyzenBody::new))
        };

        Box::pin(fut)
    }
}
