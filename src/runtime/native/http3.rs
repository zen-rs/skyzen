//! HTTP/3 on the per-core threads, beside the TLS TCP listener.
//!
//! QUIC runs on UDP, on the port the TLS listener has on TCP, so a client that learns of it from
//! the `Alt-Svc` header on a TLS response reaches the same host and port. Each serving thread
//! runs one `http-kit-h3` endpoint on its own compio runtime, next to its TCP accept loop: the
//! same router, the same drain, the same certificate and key (read once, by [`super::tls`]).
//!
//! Both protocols are bound together, before any thread starts, so a port UDP refuses fails the
//! start. An ephemeral listen port (`--port 0`) is chosen so both can take it (see [`pair`]); a
//! port the operator named is taken on both or the start fails naming the refused protocol.
//!
//! # Which threads serve QUIC
//!
//! Where `SO_REUSEPORT` balances (Linux, Android) every thread binds the UDP port itself and the
//! kernel hashes each client's address to one of them, so a connection's datagrams always reach
//! the endpoint that holds it. Elsewhere the kernel does not hash UDP to a listener, and sharing
//! one socket between threads would scatter one connection's datagrams over endpoints that know
//! nothing of it, so a single socket is bound and the first serving thread alone runs the QUIC
//! endpoint; the others serve TCP only.
//!
//! Without the `http3` feature [`Http3`] has no values, so a request for HTTP/3 is a refusal to
//! start and nothing in this module compiles any QUIC code.
//!
//! # `OpenSSL`
//!
//! quiche's TLS is `BoringSSL`, linked statically and without a symbol prefix. Wherever
//! `native-tls` means `OpenSSL` (every target but Apple's and Windows), `tls-native-tls` would
//! link `OpenSSL` into the same binary, and the two export the same `SSL_*` symbols, so the build
//! refuses the pair rather than leave the linker to mix them.

#[cfg(all(
    feature = "http3",
    feature = "tls-native-tls",
    not(any(target_vendor = "apple", windows))
))]
compile_error!(
    "the `http3` and `tls-native-tls` features cannot be combined on this target: quiche's \
     BoringSSL and native-tls's OpenSSL export the same symbols; use `tls-rustls` for the database \
     connections"
);

use std::future::Future;

use super::{busy_poll::BusyPoll, tls::ServerTls, ListenAddr, RuntimeFailure};

/// The largest request header or trailer section QUIC accepts, in bytes: what the HTTP/1.1 server
/// accepts as a message head by default.
#[cfg(feature = "http3")]
const MAX_FIELD_SECTION_SIZE: u64 = 64 * 1024;

/// Why HTTP/3 cannot be served.
#[derive(Debug)]
pub(super) enum Http3Error {
    /// HTTP/3 was requested from a binary built without the `http3` feature.
    #[cfg(not(feature = "http3"))]
    FeatureMissing,
    /// HTTP/3 was requested without TLS: QUIC has no cleartext mode.
    #[cfg(feature = "http3")]
    NoTls,
    /// HTTP/3 was requested for a unix socket, which has no UDP port to serve it on.
    #[cfg(feature = "http3")]
    NotTcp,
    /// HTTP/3 was requested for a serverless host, which terminates TLS and QUIC upstream.
    #[cfg(feature = "http3")]
    NotServer,
    /// The UDP port could not be bound.
    #[cfg(feature = "http3")]
    Bind {
        /// The address the bind was for.
        addr: std::net::SocketAddr,
        /// What the system said.
        error: std::io::Error,
    },
    /// The bound socket could not be adopted by the thread's runtime.
    #[cfg(feature = "http3")]
    Register(std::io::Error),
    /// The QUIC endpoint could not be built from the certificate and key.
    #[cfg(feature = "http3")]
    Endpoint(http_kit_h3::Error),
}

impl std::fmt::Display for Http3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(not(feature = "http3"))]
            Self::FeatureMissing => write!(
                f,
                "HTTP/3 is requested (--http3 or SKYZEN_HTTP3), but this binary was built without \
                 skyzen's `http3` feature; add `features = [\"http3\"]` to the skyzen dependency"
            ),
            #[cfg(feature = "http3")]
            Self::NoTls => write!(
                f,
                "HTTP/3 is requested (--http3 or SKYZEN_HTTP3), but TLS is not configured; QUIC \
                 has no cleartext mode, so pass --tls-cert and --tls-key"
            ),
            #[cfg(feature = "http3")]
            Self::NotTcp => write!(
                f,
                "HTTP/3 is requested (--http3 or SKYZEN_HTTP3), but the listener is a unix \
                 socket, which has no UDP port to serve QUIC on"
            ),
            #[cfg(feature = "http3")]
            Self::NotServer => write!(
                f,
                "HTTP/3 is requested (--http3 or SKYZEN_HTTP3), but this platform terminates \
                 TLS and QUIC upstream; unset it or run where it applies"
            ),
            #[cfg(feature = "http3")]
            Self::Bind { addr, error } => {
                let port = addr.port();
                write!(
                    f,
                    "could not bind UDP {addr} for HTTP/3, the port the TCP listener has: "
                )?;
                if error.kind() == std::io::ErrorKind::AddrInUse {
                    write!(f, "another socket holds udp/{port}")?;
                } else if refused_by_windows(error) {
                    write!(
                        f,
                        "Windows refuses udp/{port} to this process, which a bind is told when the \
                         port lies in a range excluded for UDP (`netsh int ipv4 show \
                         excludedportrange protocol=udp` lists them) or another socket holds it \
                         with SO_EXCLUSIVEADDRUSE; choose a port outside those ranges"
                    )?;
                } else {
                    write!(f, "the system refuses it")?;
                }
                write!(f, " ({error})")
            }
            #[cfg(feature = "http3")]
            Self::Register(error) => {
                write!(f, "could not register the HTTP/3 UDP socket: {error}")
            }
            #[cfg(feature = "http3")]
            Self::Endpoint(error) => write!(f, "the HTTP/3 endpoint cannot start: {error}"),
        }
    }
}

impl std::error::Error for Http3Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(feature = "http3")]
            Self::Bind { error, .. } | Self::Register(error) => Some(error),
            #[cfg(feature = "http3")]
            Self::Endpoint(error) => Some(error),
            _ => None,
        }
    }
}

/// What every serving thread builds its QUIC endpoint from: the settings, certificate and key
/// shared by all of them. One retry-token key is part of it, so an address-validation token
/// minted by one thread is honoured by any other.
#[cfg(feature = "http3")]
#[derive(Debug)]
pub(super) struct Http3 {
    config: http_kit_h3::ServerConfig,
}

/// Without the `http3` feature the type is uninhabited, which is what makes the `None` the launch
/// path hands the threads a fact rather than a promise.
#[cfg(not(feature = "http3"))]
#[derive(Debug)]
pub(super) struct Http3(std::convert::Infallible);

/// One thread's QUIC endpoint, ready to serve.
#[cfg(feature = "http3")]
#[derive(Debug)]
pub(super) struct Http3Server(
    http_kit_h3::Server<http_kit_io::compio::UdpSocket, http_kit_io::compio::Compio>,
);

/// Uninhabited without the `http3` feature, like [`Http3`].
#[cfg(not(feature = "http3"))]
#[derive(Debug)]
pub(super) struct Http3Server(std::convert::Infallible);

/// Decide whether HTTP/3 is served, or refuse the start when it was requested and cannot be.
///
/// It is served whenever TLS is configured on a TCP listener and the feature is compiled in;
/// `requested` (`--http3` or `SKYZEN_HTTP3`) is the operator insisting on it, which turns every
/// case where it cannot be served into an error naming the cause rather than a TLS-only server.
#[cfg(feature = "http3")]
pub(super) fn build(
    requested: bool,
    tls: Option<&ServerTls>,
    addr: &ListenAddr,
) -> Result<Option<Http3>, RuntimeFailure> {
    if requested && !matches!(addr, ListenAddr::Tcp(_)) {
        return Err(RuntimeFailure::Http3(Http3Error::NotTcp));
    }
    match tls {
        Some(tls) => Ok(Some(Http3 {
            config: http_kit_h3::ServerConfig::new(tls.identity(), MAX_FIELD_SECTION_SIZE),
        })),
        None if requested => Err(RuntimeFailure::Http3(Http3Error::NoTls)),
        None => Ok(None),
    }
}

/// Without the `http3` feature a request for HTTP/3 refuses the start and names the feature to
/// add; the alternative is a server without the protocol it was told to serve.
#[cfg(not(feature = "http3"))]
pub(super) const fn build(
    requested: bool,
    _tls: Option<&ServerTls>,
    _addr: &ListenAddr,
) -> Result<Option<Http3>, RuntimeFailure> {
    if requested {
        Err(RuntimeFailure::Http3(Http3Error::FeatureMissing))
    } else {
        Ok(None)
    }
}

/// The refusal for HTTP/3 requested where something else terminates TLS and QUIC.
#[cfg(feature = "http3")]
pub(super) const fn not_a_server() -> RuntimeFailure {
    RuntimeFailure::Http3(Http3Error::NotServer)
}

/// The refusal for HTTP/3 requested where something else terminates TLS and QUIC.
#[cfg(not(feature = "http3"))]
pub(super) const fn not_a_server() -> RuntimeFailure {
    RuntimeFailure::Http3(Http3Error::FeatureMissing)
}

/// The UDP sockets of every serving thread, in thread order; `None` for a thread that does not
/// run QUIC.
type UdpSockets = Vec<Option<std::net::UdpSocket>>;

#[cfg(feature = "http3")]
impl Http3 {
    /// Bind TCP on `addr` with `tcp`, which returns its listeners and the address they got, and
    /// then one UDP socket per serving thread on that same address, or `None` for the threads that
    /// do not run QUIC; see the module documentation for which are which. With `busy_poll`, each
    /// UDP socket carries its socket options.
    ///
    /// # Errors
    ///
    /// TCP cannot be bound, or UDP cannot be bound on the TCP port; see [`pair`] for when an
    /// ephemeral port is given up for another instead.
    #[expect(
        clippy::unused_self,
        reason = "the call exists without the feature too, where the receiver proves it unreachable"
    )]
    pub(super) fn bind_beside<T>(
        &self,
        addr: std::net::SocketAddr,
        threads: std::num::NonZeroUsize,
        busy_poll: Option<&BusyPoll>,
        tcp: impl FnMut(std::net::SocketAddr) -> std::io::Result<(T, std::net::SocketAddr)>,
    ) -> Result<(T, UdpSockets), RuntimeFailure> {
        pair(addr, tcp, |local| {
            bind_sockets(local, threads.get(), busy_poll)
        })
    }

    /// The QUIC endpoint of the calling thread, over its `socket`. Runs inside the thread's
    /// runtime, which the socket is registered with through `io`, the thread's driver.
    pub(super) fn server(
        &self,
        socket: std::net::UdpSocket,
        io: &http_kit_io::compio::Compio,
    ) -> Result<Http3Server, Http3Error> {
        use http_kit_io::FromStd as _;

        let socket = io.udp_socket(socket).map_err(Http3Error::Register)?;
        http_kit_h3::Server::new(socket, io.clone(), &self.config)
            .map(Http3Server)
            .map_err(Http3Error::Endpoint)
    }
}

#[cfg(not(feature = "http3"))]
impl Http3 {
    pub(super) const fn bind_beside<T>(
        &self,
        _addr: std::net::SocketAddr,
        _threads: std::num::NonZeroUsize,
        _busy_poll: Option<&BusyPoll>,
        _tcp: impl FnMut(std::net::SocketAddr) -> std::io::Result<(T, std::net::SocketAddr)>,
    ) -> Result<(T, UdpSockets), RuntimeFailure> {
        match self.0 {}
    }

    pub(super) const fn server(
        &self,
        _socket: std::net::UdpSocket,
        _io: &http_kit_io::compio::Compio,
    ) -> Result<Http3Server, Http3Error> {
        match self.0 {}
    }
}

/// Bind TCP on `addr` with `tcp`, then UDP on the port TCP got with `udp`.
///
/// A port free on TCP need not be free on UDP: another socket may hold the UDP port, and Windows
/// keeps a separate table of excluded port ranges per protocol, which the allocator of an
/// ephemeral TCP port does not consult for UDP. So when `addr` asks the system for a port (port
/// `0`) and UDP refuses the one TCP got for one of those reasons, that pair is set aside and TCP
/// asks again. Every refused pair stays bound until a port is found, so the system cannot hand
/// the same port out twice: the search ends at a port both protocols take, or when TCP itself
/// can get none because the ephemeral range is exhausted, which fails the start like any TCP
/// bind failure. A port the operator named is never exchanged: its refusal is the error.
#[cfg(feature = "http3")]
fn pair<T, U>(
    addr: std::net::SocketAddr,
    mut tcp: impl FnMut(std::net::SocketAddr) -> std::io::Result<(T, std::net::SocketAddr)>,
    mut udp: impl FnMut(std::net::SocketAddr) -> std::io::Result<U>,
) -> Result<(T, U), RuntimeFailure> {
    let mut refused = Vec::new();
    loop {
        let (listeners, local) = tcp(addr)?;
        match udp(local) {
            Ok(sockets) => {
                // Only now may the refused ports go back to the system.
                drop(refused);
                return Ok((listeners, sockets));
            }
            Err(error) if addr.port() == 0 && unavailable(&error) => {
                tracing::debug!(
                    port = local.port(),
                    "the system's TCP port cannot be had on UDP ({error}); asking for another"
                );
                refused.push(listeners);
            }
            Err(error) => {
                return Err(RuntimeFailure::Http3(Http3Error::Bind {
                    addr: local,
                    error,
                }));
            }
        }
    }
}

/// Whether a UDP bind failed because the port cannot be had, rather than because of the socket.
#[cfg(feature = "http3")]
fn unavailable(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::AddrInUse || refused_by_windows(error)
}

/// `WSAEACCES`, from Winsock2.h ("Windows Sockets Error Codes"). A bind gets it for a port in a
/// range excluded for the socket's protocol, or one another socket holds with
/// `SO_EXCLUSIVEADDRUSE`.
#[cfg(all(feature = "http3", windows))]
const WSAEACCES: i32 = 10013;

/// Whether Windows refused the bind with `WSAEACCES`; see [`WSAEACCES`].
#[cfg(all(feature = "http3", windows))]
fn refused_by_windows(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(WSAEACCES)
}

/// Only Windows has excluded port ranges.
#[cfg(all(feature = "http3", not(windows)))]
const fn refused_by_windows(_error: &std::io::Error) -> bool {
    false
}

#[cfg(all(feature = "http3", any(target_os = "linux", target_os = "android")))]
fn bind_sockets(
    local: std::net::SocketAddr,
    threads: usize,
    busy_poll: Option<&BusyPoll>,
) -> std::io::Result<Vec<Option<std::net::UdpSocket>>> {
    use socket2::{Domain, Protocol, Socket, Type};

    (0..threads)
        .map(|_| {
            let socket = Socket::new(Domain::for_address(local), Type::DGRAM, Some(Protocol::UDP))?;
            socket.set_reuse_address(true)?;
            socket.set_reuse_port(true)?;
            // Before `bind`, so no datagram is received without them.
            #[cfg(target_os = "linux")]
            if let Some(busy_poll) = busy_poll {
                busy_poll.set_on(&socket)?;
            }
            #[cfg(not(target_os = "linux"))]
            let _ = busy_poll;
            socket.bind(&local.into())?;
            Ok(Some(socket.into()))
        })
        .collect()
}

#[cfg(all(
    feature = "http3",
    not(any(target_os = "linux", target_os = "android"))
))]
fn bind_sockets(
    local: std::net::SocketAddr,
    threads: usize,
    _busy_poll: Option<&BusyPoll>,
) -> std::io::Result<Vec<Option<std::net::UdpSocket>>> {
    let mut sockets = Vec::with_capacity(threads);
    sockets.push(Some(std::net::UdpSocket::bind(local)?));
    sockets.resize_with(threads, || None);
    Ok(sockets)
}

#[cfg(feature = "http3")]
impl Http3Server {
    /// Answer requests with `endpoint` until `shutdown` completes, then drain: every connection
    /// is sent a GOAWAY, its open requests finish, and it closes once it has none. Returns when
    /// the last connection has.
    pub(super) async fn serve<E>(self, endpoint: E, shutdown: impl Future<Output = ()>)
    where
        E: crate::Endpoint + Clone + 'static,
    {
        let service = |request: crate::Request| {
            let endpoint = endpoint.clone();
            answer(endpoint, request)
        };
        if let Err(error) = self.0.serve_until(&service, shutdown).await {
            tracing::error!("the HTTP/3 endpoint failed: {error}");
        }
    }
}

#[cfg(not(feature = "http3"))]
impl Http3Server {
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "the call exists without the feature too, where no value reaches it"
    )]
    pub(super) async fn serve<E>(self, _endpoint: E, _shutdown: impl Future<Output = ()>)
    where
        E: crate::Endpoint + Clone + 'static,
    {
        match self.0 {}
    }
}

/// One request: its connection's peer becomes the `PeerAddr` the extractors read, and an
/// endpoint error becomes the response every other server turns it into.
#[cfg(feature = "http3")]
async fn answer<E>(mut endpoint: E, mut request: crate::Request) -> crate::Response
where
    E: crate::Endpoint + 'static,
{
    use skyzen_core::{apply_pending_headers, error_response, log_endpoint_error, PeerAddr};

    let peer = request
        .extensions()
        .get::<http_kit_h3::ConnectionInfo>()
        .expect("http-kit-h3 attaches a ConnectionInfo to every request")
        .peer_addr();
    request.extensions_mut().insert(PeerAddr::new(peer));
    let method = request.method().clone();
    let path = request.uri().path().to_owned();

    match endpoint.respond(&mut request).await {
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
    }
}

#[cfg(all(test, feature = "http3"))]
mod tests {
    use std::{
        convert::Infallible,
        io::{Read as _, Write as _},
        net::SocketAddr,
        num::NonZeroUsize,
        sync::Arc,
    };

    use async_channel::bounded;
    use bytes::Bytes;
    use executor_core::{smol::SmolGlobal, AnyExecutor};
    use futures_lite::future::zip;
    use http_kit::{Body, Method, Request, StatusCode};
    use http_kit_h3::{Client, ClientConfig, Roots};
    use http_kit_io::{compio::Compio, Datagram as _, DatagramSocket as _};
    use skyzen_hyper::AltSvc;

    use super::{build, Http3Error};
    use crate::{
        extract::{ClientIp, PeerAddr},
        routing::{CreateRouteNode, Route, Router},
        runtime::native::{
            cores::{bind, start, Cores, Http3Plan, Plan},
            tls, HttpServer, ListenAddr, RuntimeFailure, TlsFiles,
        },
    };

    /// Chinese, Japanese and Korean, so a body that is not ASCII crosses QUIC intact.
    const CJK: [&str; 3] = ["你好，世界。", "こんにちは、世界。", "안녕하세요, 세계."];

    /// What the routes answer `/who` with: the address the connection came from, as the
    /// `PeerAddr` extractor and as the `ClientIp` extractor resolve it.
    fn router() -> Router {
        Route::new((
            "/who".at(|peer: PeerAddr, ip: ClientIp| async move { format!("{} {}", peer.0, ip.0) }),
            "/echo".post(|body: Body| async move { body }),
        ))
        .build()
    }

    struct Pki {
        _dir: tempfile::TempDir,
        files: TlsFiles,
        cert_pem: String,
    }

    fn pki() -> Pki {
        let certified =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("rcgen");
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        let cert_pem = certified.cert.pem();
        std::fs::write(&cert, &cert_pem).expect("write cert");
        std::fs::write(&key, certified.signing_key.serialize_pem()).expect("write key");
        Pki {
            _dir: dir,
            files: TlsFiles { cert, key },
            cert_pem,
        }
    }

    fn loopback() -> ListenAddr {
        ListenAddr::Tcp("127.0.0.1:0".parse().unwrap())
    }

    /// The serving threads of one TLS listener with HTTP/3 beside it, and what stops them.
    struct Serving {
        addr: SocketAddr,
        stop: async_channel::Sender<Infallible>,
        drain: async_channel::Sender<Infallible>,
        drained: async_channel::Receiver<Infallible>,
        release: async_channel::Sender<Infallible>,
        cores: Cores,
    }

    impl Serving {
        fn start(pki: &Pki) -> Self {
            Self::serve(pki, |_: crate::runtime::ServingThread| async { router() })
        }

        /// Serve the routers `factory` builds, one per thread.
        fn serve(
            pki: &Pki,
            factory: impl crate::EndpointFactory<crate::runtime::ServingThread>,
        ) -> Self {
            let threads = NonZeroUsize::new(2).unwrap();
            let addr = loopback();
            let tls = tls::build(Some(&pki.files), &addr).expect("the pair loads");
            let http3 = build(true, tls.as_ref(), &addr)
                .expect("HTTP/3 is served")
                .expect("with TLS, it is");
            let bound = bind(
                &addr,
                threads,
                crate::runtime::native::steering::Pins::default(),
                None,
                Some(&http3),
            )
            .expect("bind TCP and UDP on one port");
            let local = bound.sockets[0].tcp_addr().unwrap().expect("local addr");

            let (stop, stopping) = bounded::<Infallible>(1);
            let (drain, drained) = bounded::<Infallible>(1);
            let (release, released) = bounded::<Infallible>(1);
            let max_connections = NonZeroUsize::new(64).unwrap();
            let options = crate::runtime::native::IoOptions::default();
            let plan = Plan {
                factory,
                wrap: Ok,
                executor: Arc::new(AnyExecutor::new(SmolGlobal)),
                max_connections,
                io: crate::runtime::native::IoSettings::new(
                    options.driver(),
                    options.recv_ring(max_connections),
                    None,
                ),
                server: HttpServer::H1,
                tls,
                http3: Some(Http3Plan {
                    http3,
                    alt_svc: AltSvc::h3(local.port()),
                }),
            };
            let cores =
                start(plan, bound, &stopping, &drain, &released).expect("start the threads");
            for _ in 0..cores.threads {
                cores
                    .ready
                    .recv_blocking()
                    .expect("a thread reports")
                    .expect("and is serving");
            }
            Self {
                addr: local,
                stop,
                drain,
                drained,
                release,
                cores,
            }
        }

        /// Stop every thread and wait until each has exited and released its drain token.
        fn stop(self) {
            drop(self.stop);
            drop(self.drain);
            assert!(self.drained.recv_blocking().is_err());
            drop(self.release);
            assert!(self.cores.exited.recv_blocking().is_err());
        }
    }

    fn client(pki: &Pki) -> Client {
        Client::new(&ClientConfig::new(
            Roots {
                system: false,
                pem: vec![pki.cert_pem.clone().into_bytes()],
            },
            64 * 1024,
        ))
        .expect("an h3 client")
    }

    fn request(port: u16, method: Method, path: &str, body: Body) -> Request {
        let mut request = Request::new(body);
        *request.method_mut() = method;
        *request.uri_mut() = format!("https://localhost:{port}{path}").parse().unwrap();
        request
    }

    fn on_compio<F: std::future::Future>(future: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(future)
    }

    /// An `http-kit-h3` client reaches the UDP endpoint on the TLS port: a request body that
    /// arrives in pieces comes back in pieces, and `PeerAddr` and `ClientIp` name the client.
    #[test]
    fn an_h3_client_streams_a_body_and_the_peer_resolves() {
        let pki = pki();
        let serving = Serving::start(&pki);
        let port = serving.addr.port();
        let mut client = client(&pki);

        on_compio(async {
            let io = Compio::default();
            let socket = io.bind_udp("127.0.0.1:0".parse().unwrap()).await.unwrap();
            let own = socket.local_addr().unwrap();
            let (send, connection) = client
                .connect(socket, io, "localhost", serving.addr)
                .expect("an h3 connection");
            // Owns `send`, so the connection ends when the last request has been answered.
            let exchange = async move {
                let who = send
                    .send(request(port, Method::GET, "/who", Body::empty()))
                    .await
                    .expect("/who");
                assert_eq!(who.status(), StatusCode::OK);
                let who = who.into_body().into_bytes().await.expect("body");
                assert_eq!(
                    String::from_utf8(who.to_vec()).unwrap(),
                    format!("{own} {}", own.ip())
                );

                let pieces = futures_lite::stream::iter(
                    CJK.map(|piece| Ok::<_, std::io::Error>(Bytes::from(piece))),
                );
                let echoed = send
                    .send(request(
                        port,
                        Method::POST,
                        "/echo",
                        Body::from_stream(pieces),
                    ))
                    .await
                    .expect("/echo");
                assert_eq!(echoed.status(), StatusCode::OK);
                let echoed = echoed.into_body().into_bytes().await.expect("body");
                assert_eq!(String::from_utf8(echoed.to_vec()).unwrap(), CJK.concat());
            };
            let ((), ran) = zip(exchange, connection.run()).await;
            ran.expect("the connection ends cleanly");
        });

        serving.stop();
    }

    /// A TLS TCP response tells the client where HTTP/3 is, on the port it is already using.
    #[test]
    fn tls_tcp_responses_advertise_http3_on_the_same_port() {
        use rustls::pki_types::pem::PemObject as _;

        let pki = pki();
        let serving = Serving::start(&pki);

        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls::pki_types::CertificateDer::pem_slice_iter(pki.cert_pem.as_bytes()) {
            roots.add(cert.expect("a certificate")).expect("a root");
        }
        let mut config = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut connection = rustls::ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
        )
        .expect("a TLS client");
        let mut tcp = std::net::TcpStream::connect(serving.addr).expect("connect");
        let mut tls = rustls::Stream::new(&mut connection, &mut tcp);
        tls.write_all(b"GET /who HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("write");
        let mut response = Vec::new();
        // The server closes without a close_notify on some paths; the bytes read are the answer.
        let _ = tls.read_to_end(&mut response);
        let head = String::from_utf8_lossy(&response).to_lowercase();
        assert!(head.starts_with("http/1.1 200"), "{head}");
        let expected = format!("alt-svc: h3=\":{}\"; ma=86400", serving.addr.port());
        assert!(head.contains(&expected), "no `{expected}` in:\n{head}");

        serving.stop();
    }

    /// Shutdown sends a GOAWAY on the open QUIC connection, which closes once it has no request
    /// left, and the serving threads then end: the drain waits for QUIC like it does for TCP.
    #[test]
    fn shutdown_drains_an_open_h3_connection() {
        let pki = pki();
        let serving = Serving::start(&pki);
        let port = serving.addr.port();
        let addr = serving.addr;
        let (answered_tx, answered) = std::sync::mpsc::channel::<()>();

        let client_thread = std::thread::spawn(move || {
            let mut client = client(&pki);
            on_compio(async move {
                let io = Compio::default();
                let socket = io.bind_udp("127.0.0.1:0".parse().unwrap()).await.unwrap();
                let (send, connection) = client
                    .connect(socket, io, "localhost", addr)
                    .expect("an h3 connection");
                let exchange = async {
                    let response = send
                        .send(request(port, Method::GET, "/who", Body::empty()))
                        .await
                        .expect("/who");
                    response.into_body().into_bytes().await.expect("body");
                    answered_tx.send(()).unwrap();
                };
                // `send` stays alive until the server closes the connection itself.
                let ((), ran) = zip(exchange, connection.run()).await;
                drop(send);
                ran
            })
        });

        answered.recv().expect("the request was answered");
        serving.stop();
        client_thread
            .join()
            .expect("the client thread")
            .expect("the server closed the connection gracefully");
    }

    #[test]
    fn http3_requested_without_tls_is_refused() {
        let error = build(true, None, &loopback()).expect_err("no TLS, no QUIC");
        assert!(
            matches!(error, RuntimeFailure::Http3(Http3Error::NoTls)),
            "{error}"
        );
        assert!(
            error.to_string().contains("TLS is not configured"),
            "{error}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn http3_requested_on_a_unix_listener_is_refused() {
        let addr = ListenAddr::Unix("/tmp/skyzen-h3-test.sock".into());
        let error = build(true, None, &addr).expect_err("a unix socket has no UDP port");
        assert!(
            matches!(error, RuntimeFailure::Http3(Http3Error::NotTcp)),
            "{error}"
        );
        assert!(error.to_string().contains("unix socket"), "{error}");
    }

    #[test]
    fn http3_is_not_served_without_tls_unless_it_was_requested() {
        assert!(build(false, None, &loopback())
            .expect("no refusal")
            .is_none());
    }

    #[test]
    fn http3_is_served_with_tls_whether_or_not_it_was_requested() {
        let pki = pki();
        let addr = loopback();
        let tls = tls::build(Some(&pki.files), &addr).expect("the pair loads");
        for requested in [false, true] {
            assert!(build(requested, tls.as_ref(), &addr)
                .expect("no refusal")
                .is_some());
        }
    }

    /// An HTTP/1.1 origin behind the reverse proxy, reached by an HTTP/3 client: what the origin
    /// saw is returned as response fields, and the body (trailers included) is echoed back.
    #[cfg(feature = "proxy")]
    mod proxied {
        use std::convert::Infallible;

        use bytes::Bytes;
        use futures_lite::{future::zip, StreamExt as _};
        use http_kit::{
            header::{self, HeaderMap, HeaderValue},
            Body, BodyFrame, Method, Request, Response, StatusCode,
        };
        use http_kit_io::{compio::Compio, Datagram as _, FromStd as _};
        use sha2::{Digest as _, Sha256};

        use super::{client, pki, request, Serving, CJK};
        use crate::{
            proxy::{Proxy, Upstream},
            routing::{CreateRouteNode, Route},
        };

        /// One chunk of the streamed body.
        const CHUNK: usize = 64 * 1024;
        /// How many chunks it has: 4 MiB in all.
        const CHUNKS: usize = 64;
        /// A response field the origin sets, in Chinese, Japanese and Korean.
        const ORIGIN: &str = "北京 / 東京 / 서울";

        /// The origin: echoes the request body, frames and trailers alike, and reports the
        /// forwarding fields it received.
        #[derive(Clone)]
        struct Origin;

        impl crate::Endpoint for Origin {
            type Error = Infallible;

            #[expect(
                clippy::unused_async_trait_impl,
                reason = "the echo answers with the request's own body, so it awaits nothing"
            )]
            async fn respond(&mut self, request: &mut Request) -> Result<Response, Infallible> {
                let body = request.body_mut().take().expect("body untaken");
                let mut response = Response::new(body);
                let fields = response.headers_mut();
                for (name, seen) in [
                    (header::VIA, "x-seen-via"),
                    (header::HOST, "x-seen-host"),
                    (header::FORWARDED, "x-seen-forwarded"),
                    (header::TE, "x-seen-te"),
                ] {
                    if let Some(value) = request.headers().get(&name) {
                        fields.insert(seen, value.clone());
                    }
                }
                fields.insert(header::CONNECTION, HeaderValue::from_static("x-origin-hop"));
                fields.insert("x-origin-hop", HeaderValue::from_static("1"));
                // `from_static` takes visible ASCII only; a field value may carry UTF-8 octets.
                fields.insert(
                    "x-origin",
                    HeaderValue::from_bytes(ORIGIN.as_bytes()).expect("a field value"),
                );
                Ok(response)
            }
        }

        /// Every frame of `body`: its data, and its trailers if it ends with any.
        async fn read(mut body: Body) -> (Vec<u8>, Option<HeaderMap>) {
            let mut data = Vec::new();
            let mut trailers = None;
            while let Some(frame) = body.next().await {
                match frame.expect("body frame") {
                    BodyFrame::Data(bytes) => data.extend_from_slice(&bytes),
                    BodyFrame::Trailers(fields) => trailers = Some(fields),
                    other => panic!("unexpected frame {other:?}"),
                }
            }
            (data, trailers)
        }

        fn chunk(index: usize) -> Bytes {
            (0..CHUNK)
                .map(|offset| u8::try_from((index * CHUNK + offset) * 31 % 251).expect("below 251"))
                .collect::<Vec<u8>>()
                .into()
        }

        #[test]
        fn an_h3_client_streams_through_the_proxy_to_an_h1_origin() {
            let origin = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the origin");
            let origin_addr = origin.local_addr().expect("local address");
            let target = format!("http://{origin_addr}/v1")
                .parse()
                .expect("upstream URI");
            let upstream = Upstream::new(&target).expect("upstream");

            let pki = pki();
            let serving = Serving::serve(&pki, move |thread: crate::runtime::ServingThread| {
                let upstream = upstream.clone();
                async move { Route::new(("/api".mount(Proxy::new(&upstream, &thread)),)).build() }
            });
            let port = serving.addr.port();
            let mut client = client(&pki);

            super::on_compio(async {
                let io = Compio::default();
                let origin = io.tcp_listener(origin).expect("register the origin");
                let origin_io = io.clone();
                compio::runtime::spawn(async move {
                    skyzen_h1::H1::default()
                        .serve_listener(
                            origin_io,
                            compio::runtime::Runtime::current(),
                            origin,
                            Origin,
                            core::future::pending(),
                        )
                        .await;
                })
                .detach();

                let socket = io.bind_udp("127.0.0.1:0".parse().unwrap()).await.unwrap();
                let (send, connection) = client
                    .connect(socket, io, "localhost", serving.addr)
                    .expect("an h3 connection");
                let exchange = async move {
                    // A few MiB in chunks, ending in a trailer carrying its digest.
                    let mut digest = Sha256::new();
                    for index in 0..CHUNKS {
                        digest.update(chunk(index));
                    }
                    let checksum = format!("{:x}", digest.finalize());
                    let mut trailers = HeaderMap::new();
                    trailers.insert("x-sha256", HeaderValue::try_from(&checksum).unwrap());
                    let frames = futures_lite::stream::iter(0..CHUNKS)
                        .map(|index| Ok::<_, Infallible>(BodyFrame::Data(chunk(index))))
                        .chain(futures_lite::stream::once(Ok(BodyFrame::Trailers(
                            trailers,
                        ))));
                    let mut streamed =
                        request(port, Method::POST, "/api/echo", Body::from_frames(frames));
                    streamed
                        .headers_mut()
                        .insert(header::TE, HeaderValue::from_static("trailers"));
                    let response = send.send(streamed).await.expect("the streamed exchange");

                    assert_eq!(response.status(), StatusCode::OK);
                    let fields = response.headers();
                    assert_eq!(fields["x-seen-via"], "3 skyzen");
                    assert_eq!(fields[header::VIA], "1.1 skyzen");
                    assert_eq!(fields["x-seen-host"], origin_addr.to_string());
                    assert_eq!(
                        fields["x-seen-forwarded"],
                        format!("for=127.0.0.1;host=\"localhost:{port}\";proto=https")
                    );
                    assert_eq!(fields["x-seen-te"], "trailers");
                    assert_eq!(fields["x-origin"].as_bytes(), ORIGIN.as_bytes());
                    assert!(fields.get("x-origin-hop").is_none());
                    let (data, returned) = read(response.into_body()).await;
                    assert_eq!(data.len(), CHUNK * CHUNKS);
                    assert_eq!(format!("{:x}", Sha256::digest(&data)), checksum);
                    assert_eq!(
                        returned.expect("the echoed trailers")["x-sha256"],
                        checksum.as_str()
                    );

                    let cjk = send
                        .send(request(
                            port,
                            Method::POST,
                            "/api/echo",
                            Body::from(CJK.concat()),
                        ))
                        .await
                        .expect("the CJK exchange");
                    assert_eq!(cjk.status(), StatusCode::OK);
                    let (data, _) = read(cjk.into_body()).await;
                    assert_eq!(String::from_utf8(data).unwrap(), CJK.concat());
                };
                let ((), ran) = zip(exchange, connection.run()).await;
                ran.expect("the connection ends cleanly");
            });

            serving.stop();
        }
    }

    /// A named port whose UDP side something else holds fails the start, naming the address,
    /// the protocol and why; it is never exchanged for another port.
    #[test]
    fn a_named_port_refused_on_udp_fails_the_start_saying_why() {
        let pki = pki();
        let tls = tls::build(Some(&pki.files), &loopback()).expect("the pair loads");
        let held = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let local = held.local_addr().unwrap();
        let addr = ListenAddr::Tcp(local);
        let http3 = build(true, tls.as_ref(), &addr)
            .expect("no refusal")
            .expect("served");

        let error = bind(
            &addr,
            NonZeroUsize::new(2).unwrap(),
            crate::runtime::native::steering::Pins::default(),
            None,
            Some(&http3),
        )
        .expect_err("the UDP port is taken");
        assert!(
            matches!(error, RuntimeFailure::Http3(Http3Error::Bind { addr, .. }) if addr == local),
            "{error}"
        );
        let message = error.to_string();
        assert!(
            message.contains(&format!("could not bind UDP {local} for HTTP/3")),
            "{message}"
        );
        assert!(
            message.contains(&format!("another socket holds udp/{}", local.port())),
            "{message}"
        );
    }

    /// When the port TCP was given cannot be had on UDP, both move to another one the system
    /// gives, and the refused one stays held until then so it is not given again.
    #[test]
    fn an_ephemeral_port_refused_on_udp_moves_both_protocols_to_another() {
        let mut blocker = None;
        let mut tried = Vec::new();
        let (listener, sockets) = super::pair(
            "127.0.0.1:0".parse().unwrap(),
            |addr| {
                let listener = std::net::TcpListener::bind(addr)?;
                let local = listener.local_addr()?;
                tried.push(local.port());
                Ok((listener, local))
            },
            |local| {
                // The first port's UDP side is taken before the server gets to it. Where the
                // system already refuses that port on UDP (a Windows excluded range), holding it
                // fails and the refusal is the system's own.
                if blocker.is_none() {
                    blocker = Some(std::net::UdpSocket::bind(local).ok());
                }
                super::bind_sockets(local, 1, None)
            },
        )
        .expect("a port both protocols take");

        assert!(tried.len() >= 2, "the refused port was kept: {tried:?}");
        let distinct: std::collections::BTreeSet<_> = tried.iter().collect();
        assert_eq!(
            distinct.len(),
            tried.len(),
            "a refused port was given again"
        );
        let port = listener.local_addr().unwrap().port();
        assert_eq!(Some(&port), tried.last());
        let udp = sockets[0].as_ref().expect("the first thread runs QUIC");
        assert_eq!(udp.local_addr().unwrap().port(), port);
    }

    /// On Windows, `WSAEACCES` on the UDP bind names the excluded ranges and how to list them.
    #[cfg(windows)]
    #[test]
    fn a_windows_udp_refusal_names_the_excluded_ranges() {
        let addr: SocketAddr = "127.0.0.1:53766".parse().unwrap();
        let error = Http3Error::Bind {
            addr,
            error: std::io::Error::from_raw_os_error(super::WSAEACCES),
        };
        let message = error.to_string();
        assert!(message.contains("a range excluded for UDP"), "{message}");
        assert!(
            message.contains("netsh int ipv4 show excludedportrange protocol=udp"),
            "{message}"
        );
        assert!(super::unavailable(&std::io::Error::from_raw_os_error(
            super::WSAEACCES
        )));
    }

    /// With busy polling on, every thread's QUIC socket carries the busy-poll options.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_quic_socket_carries_the_busy_poll_options() {
        use crate::runtime::native::busy_poll::{read_socket_options, BusyPoll, BusyPollTimeout};

        let busy_poll = BusyPoll {
            timeout: BusyPollTimeout::from_micros(50).unwrap(),
            prefer: false,
            budget: None,
        };
        let sockets =
            super::bind_sockets("127.0.0.1:0".parse().unwrap(), 2, Some(&busy_poll)).expect("bind");
        assert_eq!(sockets.len(), 2);
        for socket in sockets {
            let socket = socket.expect("every Linux thread runs QUIC");
            assert_eq!(read_socket_options(&socket), (50, false));
        }
    }
}

#[cfg(all(test, not(feature = "http3")))]
mod tests {
    use super::build;
    use crate::runtime::native::{ListenAddr, RuntimeFailure};

    /// Without `http3` a request for it is refused and names the feature — it must never start a
    /// server without the protocol it was told to serve.
    #[test]
    fn http3_requested_without_the_feature_is_refused() {
        let addr = ListenAddr::Tcp("127.0.0.1:0".parse().unwrap());
        let error = build(true, None, &addr).expect_err("the feature is missing");
        assert!(matches!(error, RuntimeFailure::Http3(_)), "{error}");
        assert!(error.to_string().contains("`http3` feature"), "{error}");
    }

    #[test]
    fn nothing_is_refused_when_http3_was_not_requested() {
        let addr = ListenAddr::Tcp("127.0.0.1:0".parse().unwrap());
        assert!(build(false, None, &addr).expect("no refusal").is_none());
    }
}
