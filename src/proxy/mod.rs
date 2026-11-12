//! A reverse-proxy endpoint: requests that reach it are forwarded to one upstream origin, and its
//! answers streamed back.
//!
//! An [`Upstream`] is the recipe: where requests go, how connections to it are made, and how the
//! forwarded messages are rewritten. It is `Send + Sync`, built once and handed to every serving
//! thread. A [`Proxy`] is what one thread serves with: the endpoint, over a connection pool that
//! lives on that thread and is never shared with another.
//!
//! ```rust,no_run
//! use skyzen::{
//!     proxy::{Proxy, Upstream},
//!     routing::{CreateRouteNode, Route, Router},
//! };
//!
//! // Once, before the serving threads start.
//! let upstream = Upstream::new(&"http://127.0.0.1:9000/v1".parse()?)?;
//!
//! // On each serving thread, inside the endpoint factory.
//! let router: Router = Route::new(("/api".mount(Proxy::new(&upstream)),)).build();
//! // `/api/users?page=2` is forwarded as `http://127.0.0.1:9000/v1/users?page=2`.
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! What a forwarded request carries:
//!
//! - The hop-by-hop fields (RFC 9110 §7.6.1) are removed in both directions, `Connection` and
//!   whatever it nominates included.
//! - `Host` names the upstream, unless [`Upstream::preserve_host`] keeps the client's.
//! - The connection's peer is recorded as a `Forwarded` element (RFC 7239), with the `Host` the
//!   client asked for and, where the request carries it, its scheme. The forwarding fields the
//!   request arrived with — `Forwarded`, `X-Forwarded-*`, `X-Real-IP`, `CF-Connecting-IP` — are
//!   kept and extended only when a [`TrustedProxies`] layer vouches for the peer; from any other
//!   peer they are a client's claim and are removed.
//! - `Via` records this hop in both directions (RFC 9110 §7.6.3).
//!
//! Bodies are streamed in both directions, frame by frame as each side reads, so neither is ever
//! held whole and a slow reader on one side slows the writer on the other. Trailer fields cross
//! with them: a request's go upstream as chunked trailers, and a response's reach the client when
//! it asked for them with `TE: trailers`; a response that brings trailers to a client that did not
//! ends with [`ProxyError::TrailersNotAccepted`] rather than losing them.
//!
//! On Linux, where both connections are plain sockets or kernel TLS ones, the bytes of an HTTP/1.1
//! body and of the tunnel below never enter user space: they are spliced from one socket to the
//! other through a pipe. Elsewhere, and over a TLS session in user space, they are copied.
//!
//! A request that asks to switch protocols (`Connection: upgrade` with `Upgrade`, a WebSocket
//! handshake for one) keeps those two fields on its way upstream when its connection can be handed
//! over, which is an HTTP/1.1 one. When the upstream answers `101 Switching Protocols`, the
//! client's connection and the upstream's are joined into one tunnel until both sides close; any
//! other answer is returned as it is. On a connection that cannot switch (an HTTP/2 or HTTP/3
//! stream), the request goes upstream without them, as an intermediary that does not take part in
//! an upgrade sends it (RFC 9110 §7.8). `CONNECT` is refused with [`ProxyError::Connect`].

mod body;
mod headers;
mod tunnel;

use std::{cell::OnceCell, fmt, rc::Rc, sync::Arc};

use executor_core::LocalExecutor;
use http::{
    uri::{Authority, PathAndQuery, Scheme},
    Version,
};
use http_kit_io::compio::Compio;
use zenwave::{
    backend::{NativeBackend, NativeIo, NativeResolve, Upgrade},
    dns::Getaddrinfo,
};

/// The connection settings a [`Transport`] is built from.
pub use zenwave::transport;
/// How connections to an upstream are made: trusted roots, a forward proxy, a unix socket.
pub use zenwave::Transport;

use crate::{
    extract::TrustedProxies,
    header::{self, HeaderValue},
    Body, Endpoint, Request, Response, StatusCode, Uri,
};
use headers::{ForwardedElement, ViaMember};
use skyzen_core::{upgrade::UpgradeAvailable, PeerAddr};

/// The pseudonym a proxy records itself under in `Via` until [`Upstream::via`] names another.
const DEFAULT_PSEUDONYM: &str = "skyzen";

/// Where a [`Proxy`] forwards to, and how.
///
/// Built once and cloned into every serving thread, where [`Proxy::new`] turns it into that
/// thread's endpoint.
#[derive(Clone)]
pub struct Upstream {
    scheme: Scheme,
    authority: Authority,
    /// The upstream path every forwarded path is appended to, without its trailing `/`.
    base: String,
    transport: Transport,
    pseudonym: String,
    preserve_host: bool,
}

impl fmt::Debug for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upstream")
            .field("scheme", &self.scheme)
            .field("authority", &self.authority)
            .field("base", &self.base)
            .field("pseudonym", &self.pseudonym)
            .field("preserve_host", &self.preserve_host)
            .finish_non_exhaustive()
    }
}

/// Why an [`Upstream`] cannot be built.
#[skyzen::error(status = StatusCode::INTERNAL_SERVER_ERROR)]
pub enum UpstreamError {
    /// The upstream URI's scheme is neither `http` nor `https`.
    #[error("upstream `{0}` must use `http` or `https`")]
    Scheme(String),
    /// The upstream URI names no host.
    #[error("upstream `{0}` names no host")]
    MissingAuthority(String),
    /// The upstream URI carries a query, which a forwarded request's own query has no place beside.
    #[error("upstream `{0}` carries a query; only a path prefix can be prepended to a request")]
    Query(String),
    /// The `Via` pseudonym is not an RFC 9110 token.
    #[error("`{0}` is not a token, so it cannot be a `Via` pseudonym")]
    Pseudonym(String),
    /// The default transport could not be built (the system trust store failed to load).
    #[error("the upstream transport cannot be built: {0}")]
    Transport(#[from] zenwave::Error),
}

impl Upstream {
    /// Forward to `uri`: its scheme and authority name the origin, and its path, if any, is the
    /// prefix every forwarded path is appended to.
    ///
    /// Connections go straight to the origin over the platform's trust store; the system's forward
    /// proxy settings, which are for outbound traffic, are not followed. [`Upstream::transport`]
    /// replaces that.
    ///
    /// # Errors
    ///
    /// The URI is not an `http` or `https` URI with a host and no query, or the platform's trust
    /// store cannot be loaded.
    pub fn new(uri: &Uri) -> Result<Self, UpstreamError> {
        let scheme = match uri.scheme() {
            Some(scheme) if *scheme == Scheme::HTTP || *scheme == Scheme::HTTPS => scheme.clone(),
            _ => return Err(UpstreamError::Scheme(uri.to_string())),
        };
        let Some(authority) = uri.authority().cloned() else {
            return Err(UpstreamError::MissingAuthority(uri.to_string()));
        };
        if uri.query().is_some() {
            return Err(UpstreamError::Query(uri.to_string()));
        }
        let transport = Transport::builder()
            .proxy(transport::Proxy::none())
            .build()?;
        Ok(Self {
            scheme,
            authority,
            base: uri.path().trim_end_matches('/').to_owned(),
            transport,
            pseudonym: DEFAULT_PSEUDONYM.to_owned(),
            preserve_host: false,
        })
    }

    /// Make connections to the upstream through `transport`: extra trusted roots for a private
    /// CA, a forward proxy, a unix domain socket.
    #[must_use]
    pub fn transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Record this proxy in `Via` as `pseudonym` (`skyzen` until set).
    ///
    /// # Errors
    ///
    /// `pseudonym` is not an RFC 9110 token.
    pub fn via(mut self, pseudonym: &str) -> Result<Self, UpstreamError> {
        if !headers::is_token(pseudonym) {
            return Err(UpstreamError::Pseudonym(pseudonym.to_owned()));
        }
        pseudonym.clone_into(&mut self.pseudonym);
        Ok(self)
    }

    /// Forward the `Host` the client asked for instead of the upstream's authority, for an origin
    /// that serves several virtual hosts.
    #[must_use]
    pub const fn preserve_host(mut self) -> Self {
        self.preserve_host = true;
        self
    }

    /// The upstream URI a request for `path_and_query` goes to.
    fn target(&self, path_and_query: Option<&PathAndQuery>) -> Result<Uri, ProxyError> {
        let path = path_and_query.map_or("/", PathAndQuery::as_str);
        let mut joined = String::with_capacity(self.base.len() + path.len());
        joined.push_str(&self.base);
        joined.push_str(path);
        Uri::builder()
            .scheme(self.scheme.clone())
            .authority(self.authority.clone())
            .path_and_query(joined)
            .build()
            .map_err(|error| ProxyError::Target(error.to_string()))
    }
}

/// A reverse-proxy endpoint, forwarding every request to its [`Upstream`].
///
/// Its connections are pooled on the thread that built it, and clones share them; mount it with
/// [`CreateRouteNode::mount`](crate::routing::CreateRouteNode::mount) to forward everything
/// under a prefix.
#[derive(Clone)]
pub struct Proxy {
    upstream: Arc<Upstream>,
    /// The upstream pool, which [`Proxy::new`] builds on the first request, with the driver that
    /// request was served on.
    client: Rc<OnceCell<NativeBackend>>,
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("upstream", &self.upstream)
            .finish_non_exhaustive()
    }
}

impl Proxy {
    /// This thread's proxy to `upstream`, on the compio runtime `#[skyzen::main]` serves each
    /// thread with.
    ///
    /// Its pool is built on the first request, with the driver of the serving thread that request
    /// names in its `skyzen::runtime::native::IoSettings`, so upstream sockets follow
    /// the operator's zero-copy threshold and file table; a request without them (an embedding
    /// host, a test client) comes from no runtime with operator settings, and gets a default
    /// driver.
    ///
    /// # Panics
    ///
    /// At its first request, when that request is served on a thread not running a compio
    /// runtime; on another runtime use [`Proxy::on_current_thread`].
    #[must_use]
    pub fn new(upstream: &Upstream) -> Self {
        Self {
            upstream: Arc::new(upstream.clone()),
            client: Rc::new(OnceCell::new()),
        }
    }

    /// This thread's proxy to `upstream`, its connections driven by `io`, the names it dials
    /// resolved by `resolver`, and spawned on `executor`; `io` and `executor` must belong to the
    /// runtime the current thread runs.
    pub fn on_current_thread<I, R, E>(upstream: &Upstream, io: I, resolver: R, executor: &E) -> Self
    where
        I: NativeIo,
        R: NativeResolve,
        E: LocalExecutor,
    {
        Self {
            upstream: Arc::new(upstream.clone()),
            client: Rc::new(OnceCell::from(NativeBackend::on_current_thread(
                upstream.transport.clone(),
                io,
                resolver,
                executor,
            ))),
        }
    }

    /// This thread's pool, built on its first use with the driver `request` was served on.
    fn client(&self, request: &Request) -> NativeBackend {
        self.client
            .get_or_init(|| {
                let io = driver(request);
                compio::runtime::Runtime::with_current(|runtime| {
                    NativeBackend::on_current_thread(
                        self.upstream.transport.clone(),
                        io,
                        Getaddrinfo,
                        runtime,
                    )
                })
            })
            .clone()
    }

    /// The request to send upstream, built from the one the client sent; `accepts_trailers` is
    /// whether that client accepts trailer fields in the response.
    fn forwarded_request(
        &self,
        request: &mut Request,
        accepts_trailers: bool,
    ) -> Result<Request, ProxyError> {
        let settings = &*self.upstream;
        if request.method() == http::Method::CONNECT {
            return Err(ProxyError::Connect);
        }
        // The protocols the client offers to switch to, kept only where the server can hand its
        // connection over once the upstream switches.
        let offered = if headers::requests_upgrade(request.headers())
            && request.extensions().get::<UpgradeAvailable>().is_some()
        {
            headers::upgrade_fields(request.headers())
        } else {
            Vec::new()
        };

        let version = request.version();
        let mut fields = request.headers().clone();
        headers::remove_hop_by_hop(&mut fields);
        let upgrade = !offered.is_empty();
        for protocol in offered {
            fields.append(header::UPGRADE, protocol);
        }
        if version == Version::HTTP_2 || version == Version::HTTP_3 {
            headers::fold_cookies(&mut fields);
        }

        // The host the client asked for: its `Host` field, or the authority an h2 or h3 request
        // carries in its target instead.
        let asked_host = match request.headers().get(header::HOST) {
            Some(host) => Some(host.to_str().map_err(|_| ProxyError::InvalidHost)?),
            None => request.uri().authority().map(Authority::as_str),
        };
        if settings.preserve_host {
            if let Some(host) = asked_host {
                let host = HeaderValue::try_from(host).map_err(|_| ProxyError::InvalidHost)?;
                fields.insert(header::HOST, host);
            }
        } else {
            let host = HeaderValue::try_from(settings.authority.as_str())
                .expect("a parsed authority is a valid field value");
            fields.insert(header::HOST, host);
        }

        let peer = request.extensions().get::<PeerAddr>().map(|peer| peer.ip());
        let trusted = peer.is_some_and(|peer| {
            request
                .extensions()
                .get::<TrustedProxies>()
                .is_some_and(|proxies| proxies.trusts(&peer))
        });
        let element = ForwardedElement {
            client: peer,
            host: asked_host,
            proto: request.uri().scheme_str(),
        };
        headers::record_forwarding(&mut fields, trusted, &element);
        fields.append(
            header::VIA,
            ViaMember {
                version,
                pseudonym: &settings.pseudonym,
            }
            .to_value(),
        );
        // `TE` and `Upgrade` are hop-by-hop themselves, so `Connection` nominates each this hop
        // sends (RFC 9110 §7.6.1).
        let connection = match (accepts_trailers, upgrade) {
            (true, true) => Some("te, upgrade"),
            (true, false) => Some("te"),
            (false, true) => Some("upgrade"),
            (false, false) => None,
        };
        if accepts_trailers {
            // This hop accepts trailers because the client does (RFC 9110 §10.1.4).
            fields.insert(header::TE, HeaderValue::from_static("trailers"));
        }
        if let Some(connection) = connection {
            fields.insert(header::CONNECTION, HeaderValue::from_static(connection));
        }

        let body = request
            .body_mut()
            .take()
            .map_err(|_| ProxyError::BodyConsumed)?;
        let mut forwarded = Request::new(body::forward(body, false));
        *forwarded.method_mut() = request.method().clone();
        *forwarded.uri_mut() = self.upstream.target(request.uri().path_and_query())?;
        *forwarded.version_mut() = Version::HTTP_11;
        *forwarded.headers_mut() = fields;
        Ok(forwarded)
    }

    /// The response to send the client, built from the one the upstream sent.
    fn returned_response(&self, mut response: Response, accepts_trailers: bool) -> Response {
        // The backend hands over a switched connection only for the upgrade this proxy forwarded.
        let switched = Upgrade::on(&mut response);
        let chunked = headers::is_chunked(response.headers());
        let version = response.version();
        let protocols = if switched.is_some() {
            headers::upgrade_fields(response.headers())
        } else {
            Vec::new()
        };
        let fields = response.headers_mut();
        headers::remove_hop_by_hop(fields);
        if let Some(upgrade) = switched {
            // The switch is this hop's too: it names the protocol it switched to.
            for protocol in protocols {
                fields.append(header::UPGRADE, protocol);
            }
            fields.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
            response.extensions_mut().insert(tunnel::session(upgrade));
        }
        let fields = response.headers_mut();
        fields.append(
            header::VIA,
            ViaMember {
                version,
                pseudonym: &self.upstream.pseudonym,
            }
            .to_value(),
        );
        // Only a chunked body has a trailer section to bring.
        let body = core::mem::replace(response.body_mut(), Body::empty());
        *response.body_mut() = body::forward(body, chunked && !accepts_trailers);
        response
    }
}

/// The driver of the serving thread `request` came from, or a default one when it came from no
/// runtime that carries operator settings.
fn driver(request: &Request) -> Compio {
    #[cfg(feature = "rt")]
    if let Some(settings) = request
        .extensions()
        .get::<crate::runtime::native::IoSettings>()
    {
        return settings.driver();
    }
    #[cfg(not(feature = "rt"))]
    let _ = request;
    Compio::default()
}

impl Endpoint for Proxy {
    type Error = ProxyError;

    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        let accepts_trailers = headers::accepts_trailers(request.headers());
        let mut client = self.client(request);
        let mut forwarded = self.forwarded_request(request, accepts_trailers)?;
        tracing::debug!(
            method = %forwarded.method(),
            target = %forwarded.uri(),
            "forwarding upstream"
        );
        let response = match client.respond(&mut forwarded).await {
            Ok(response) => response,
            Err(zenwave::Error::Timeout(_)) => return Err(ProxyError::Timeout),
            Err(error) => return Err(ProxyError::Upstream(error)),
        };
        Ok(self.returned_response(response, accepts_trailers))
    }
}

/// Why a request could not be proxied.
#[skyzen::error]
pub enum ProxyError {
    /// The upstream could not be reached, or broke off the exchange.
    #[error("the upstream failed: {0}", status = StatusCode::BAD_GATEWAY)]
    Upstream(#[from] zenwave::Error),
    /// The upstream did not answer in time.
    #[error("the upstream did not answer in time", status = StatusCode::GATEWAY_TIMEOUT)]
    Timeout,
    /// The request is a `CONNECT`, which asks for a tunnel to a host of the client's choosing; a
    /// reverse proxy forwards only to its upstream.
    #[error(
        "a reverse proxy opens no `CONNECT` tunnel",
        status = StatusCode::NOT_IMPLEMENTED
    )]
    Connect,
    /// The request's `Host` is not visible ASCII.
    #[error("the request's `Host` is not visible ASCII", status = StatusCode::BAD_REQUEST)]
    InvalidHost,
    /// Something before the proxy already took the request body.
    #[error(
        "the request body was taken before the proxy could forward it",
        status = StatusCode::INTERNAL_SERVER_ERROR
    )]
    BodyConsumed,
    /// The forwarded path does not form an upstream URI.
    #[error("the upstream target is not a URI: {0}", status = StatusCode::BAD_REQUEST)]
    Target(String),
    /// The upstream ended its response with trailer fields, and the client did not say it accepts
    /// them with `TE: trailers`.
    #[error(
        "the upstream sent trailers the client did not accept with `TE: trailers`",
        status = StatusCode::BAD_GATEWAY
    )]
    TrailersNotAccepted,
}

#[cfg(all(test, feature = "rt"))]
mod tests {
    use core::num::NonZeroUsize;

    use super::driver;
    use crate::{Body, Request};

    /// A request the native runtime served carries its driver, and the proxy's pool is built with
    /// it: the zero-copy threshold and file table the operator changes are the ones its upstream
    /// sockets read.
    #[test]
    fn a_request_from_the_native_runtime_hands_the_proxy_its_driver() {
        use crate::runtime::native::{IoOptions, IoSettings};

        let options = IoOptions::default();
        let settings = IoSettings::new(
            options.driver(),
            options.recv_ring(NonZeroUsize::new(8).unwrap()),
            None,
        );
        let mut request = Request::new(Body::empty());
        request.extensions_mut().insert(settings.clone());

        let io = driver(&request);
        settings.zero_copy().set_threshold(12_345);
        settings.file_table().set_slots(321);

        assert_eq!(io.zero_copy().threshold(), 12_345);
        assert_eq!(io.file_table().slots(), 321);
    }
}
