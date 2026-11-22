use futures_util::TryStreamExt;
use http_body::SizeHint;
use http_body_util::BodyDataStream;
use hyper::{
    body::{Frame, Incoming},
    service::Service,
};
use std::{
    cell::Cell,
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

pub use skyzen_core::AltSvc;
use skyzen_core::{
    apply_pending_headers, error_response, header, log_endpoint_error, upgrade::OnUpgrade,
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

/// The session of an upgrade the endpoint answered with `101`, waiting for hyper to finish
/// writing the response and give the connection back.
pub type UpgradeSlot = Rc<Cell<Option<OnUpgrade>>>;

/// Hyper service adapter for skyzen endpoints.
///
/// This is the canonical adapter for embedding Skyzen into a hyper server. It injects the peer
/// address (when provided via [`IntoService::with_peer_addr`]) so the `PeerAddr`/`ClientIp`
/// extractors work, and converts handler errors into JSON HTTP responses (hiding 5xx detail)
/// instead of tearing down the connection.
///
/// Neither the endpoint nor the future the service returns has to be `Send`: hyper drives an
/// HTTP/1 connection on the task that serves it, and spawns HTTP/2 streams on whatever
/// `hyper::rt::Executor` the connection was built with.
///
/// The session of an upgrade the endpoint answers with `101` is not run by the service but by
/// [`serve_connection`](crate::serve_connection), which takes the connection back from hyper
/// once the response is written.
pub struct IntoService<E> {
    endpoint: E,
    peer_addr: Option<SocketAddr>,
    extensions: Extensions,
    alt_svc: Option<header::HeaderValue>,
    upgrade: UpgradeSlot,
}

// The endpoint is only ever cloned and called, never pinned in place, so hyper may poll a
// connection holding this service without pinning it.
impl<E> Unpin for IntoService<E> {}

impl<E> std::fmt::Debug for IntoService<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntoService")
            .field("peer_addr", &self.peer_addr)
            .finish_non_exhaustive()
    }
}

impl<E: Endpoint + Clone> IntoService<E> {
    /// Create a new service with the given endpoint.
    pub fn new(endpoint: E) -> Self {
        Self {
            endpoint,
            peer_addr: None,
            extensions: Extensions::new(),
            alt_svc: None,
            upgrade: Rc::default(),
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

    /// Advertise an HTTP/3 endpoint in every response that does not carry an `Alt-Svc` of its own.
    #[must_use]
    pub fn with_alt_svc(mut self, alt_svc: &AltSvc) -> Self {
        self.alt_svc = Some(alt_svc.header_value());
        self
    }

    /// The slot an upgrade's session is left in, shared with the service's futures.
    pub(crate) fn upgrade_slot(&self) -> UpgradeSlot {
        Rc::clone(&self.upgrade)
    }
}

impl<E> Service<hyper::Request<Incoming>> for IntoService<E>
where
    E: Endpoint + Clone + 'static,
{
    type Response = hyper::Response<SkyzenBody>;
    // Handler errors are converted into HTTP responses, so the service itself never fails.
    type Error = Infallible;
    type Future = LocalBoxFuture<Result<Self::Response, Self::Error>>;

    fn call(&self, req: hyper::Request<Incoming>) -> Self::Future {
        let mut endpoint = self.endpoint.clone();
        let peer_addr = self.peer_addr;
        let extensions = self.extensions.clone();
        let alt_svc = self.alt_svc.clone();
        let upgrade = Rc::clone(&self.upgrade);
        let fut = async move {
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
                        method = request.method().as_str(),
                        path = request.uri().path(),
                        status = response.status().as_u16(),
                        "request completed"
                    );
                    response
                }
                Err(error) => {
                    log_endpoint_error(&error, request.method(), request.uri().path());
                    let mut response = error_response(&error);
                    apply_pending_headers(&request, &mut response);
                    response
                }
            };

            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                if let Some(session) = response.extensions_mut().remove::<OnUpgrade>() {
                    upgrade.set(Some(session));
                }
            }
            if let Some(alt_svc) = alt_svc {
                response
                    .headers_mut()
                    .entry(header::ALT_SVC)
                    .or_insert(alt_svc);
            }

            Ok(response.map(SkyzenBody::new))
        };

        Box::pin(fut)
    }
}
