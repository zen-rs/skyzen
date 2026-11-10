use executor_core::{LocalExecutor, Task};
use futures_util::TryStreamExt;
use http_body::SizeHint;
use http_body_util::BodyDataStream;
use hyper::{
    body::{Frame, Incoming},
    rt::{Read as _, ReadBuf, Write as _},
    service::Service,
};
use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use http_kit::utils::{AsyncRead, AsyncWrite};
use skyzen_core::{
    apply_pending_headers, error_response, log_endpoint_error,
    upgrade::{OnUpgrade, Upgraded},
    BodyError, Endpoint, Extensions, PeerAddr, StatusCode,
};

type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;
type Bytes = http_kit::utils::Bytes;

/// Response body adapter bridging a skyzen [`Body`](skyzen_core::Body) to hyper.
///
/// It forwards the body's own `http_body::Body` view: data and trailers pass through, a file
/// region is read out into data frames (hyper cannot send a file), and the
/// [`size_hint`](http_body::Body::size_hint) is exact whenever the body knows its length, so
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
        Pin::new(&mut self.0).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        http_body::Body::size_hint(&self.0)
    }
}

/// Hyper service adapter for skyzen endpoints.
///
/// This is the canonical adapter for embedding Skyzen into a hyper server. It injects the peer
/// address (when provided via [`IntoService::with_peer_addr`]) so the `PeerAddr`/`ClientIp`
/// extractors work, converts handler errors into JSON HTTP responses (hiding 5xx detail) instead
/// of tearing down the connection, and runs the session of an upgrade the endpoint answered with
/// `101` on `executor`.
///
/// Neither the endpoint nor the future the service returns has to be `Send`: hyper drives an
/// HTTP/1 connection on the task that serves it, and spawns HTTP/2 streams on whatever
/// `hyper::rt::Executor` the connection was built with.
pub struct IntoService<E, Ex> {
    endpoint: E,
    executor: Rc<Ex>,
    peer_addr: Option<SocketAddr>,
    extensions: Extensions,
}

impl<E, Ex> std::fmt::Debug for IntoService<E, Ex> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntoService")
            .field("peer_addr", &self.peer_addr)
            .finish_non_exhaustive()
    }
}

impl<E: Endpoint + Clone, Ex: LocalExecutor> IntoService<E, Ex> {
    /// Create a new service with the given endpoint, spawning upgrade sessions on `executor`.
    pub fn new(endpoint: E, executor: Rc<Ex>) -> Self {
        Self {
            endpoint,
            executor,
            peer_addr: None,
            extensions: Extensions::new(),
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

    /// Insert a clone of every value in `extensions` into each request the service handles.
    ///
    /// This is how a runtime hands per-connection context to its handlers.
    #[must_use]
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions.extend(extensions);
        self
    }
}

impl<E, Ex> Service<hyper::Request<Incoming>> for IntoService<E, Ex>
where
    E: Endpoint + Clone + 'static,
    Ex: LocalExecutor + 'static,
{
    type Response = hyper::Response<SkyzenBody>;
    // Handler errors are converted into HTTP responses, so the service itself never fails.
    type Error = Infallible;
    type Future = LocalBoxFuture<Result<Self::Response, Self::Error>>;

    fn call(&self, mut req: hyper::Request<Incoming>) -> Self::Future {
        let mut endpoint = self.endpoint.clone();
        let executor = Rc::clone(&self.executor);
        let peer_addr = self.peer_addr;
        let extensions = self.extensions.clone();
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
            request.extensions_mut().extend(extensions);
            if let Some(peer_addr) = peer_addr {
                request.extensions_mut().insert(PeerAddr::new(peer_addr));
            }

            let mut response: skyzen_core::Response = match endpoint.respond(&mut request).await {
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
                    let mut response = error_response(&error);
                    apply_pending_headers(&request, &mut response);
                    response
                }
            };

            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                if let Some(session) = response.extensions_mut().remove::<OnUpgrade>() {
                    // hyper resolves the upgrade once the `101` has been written.
                    executor
                        .spawn_local(async move {
                            match on_upgrade.await {
                                Ok(upgraded) => {
                                    session.run(Upgraded::new(HyperUpgraded(upgraded))).await;
                                }
                                Err(error) => tracing::error!("connection upgrade failed: {error}"),
                            }
                        })
                        .detach();
                }
            }

            Ok(response.map(SkyzenBody::new))
        };

        Box::pin(fut)
    }
}

/// hyper's upgraded connection, seen through the `futures-io` traits a session reads it with.
struct HyperUpgraded(hyper::upgrade::Upgraded);

impl AsyncRead for HyperUpgraded {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut read = ReadBuf::new(buf);
        match Pin::new(&mut self.get_mut().0).poll_read(cx, read.unfilled()) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read.filled().len())),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for HyperUpgraded {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}
