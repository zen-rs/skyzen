//! TLS on the per-core threads.
//!
//! `http-kit-tls` drives rustls straight over the compio stream: records are decrypted in place in
//! the buffer the socket read into and encrypted into the buffer it writes from, with no
//! `into_poll()` copy in front of the handshake. On Linux the record layer is then handed to the
//! kernel, so a file-region body leaves through `sendfile`/`splice` already encrypted.
//!
//! ALPN picks the protocol the decrypted stream speaks. `h2` is hyper's HTTP/2 over the stream's
//! copying poll view, after the connection preface has been read; anything else is HTTP/1.1,
//! served by the server the plan selected — skyzen's own `skyzen-h1` by default, on the
//! owned-buffer stream itself, or hyper when `--server hyper` picked it.
//!
//! Without the `server-tls` feature [`ServerTls`] has no values, which is what makes "TLS was
//! configured but this build cannot serve it" a refusal to start rather than a cleartext
//! listener.

#[cfg(feature = "server-tls")]
use std::cell::Cell;
use std::{future::Future, num::NonZeroUsize, rc::Rc};

use executor_core::compio::Runtime;
use http_kit_io::{compio::Compio, Io, Listener};
use skyzen_core::Endpoint;
use std::net::SocketAddr;

#[cfg(feature = "server-tls")]
use skyzen_h1::H1;

#[cfg(feature = "server-tls")]
use super::HttpServer;
use super::{ListenAddr, RuntimeFailure, TlsFiles};

/// Why the server cannot terminate TLS.
#[derive(Debug)]
pub(super) enum TlsError {
    /// The binary was built without the `server-tls` feature.
    #[cfg(not(feature = "server-tls"))]
    FeatureMissing,
    /// TLS was requested for a listener that has no transport to encrypt.
    #[cfg(feature = "server-tls")]
    NotTcp,
    /// A certificate or key file could not be read.
    #[cfg(feature = "server-tls")]
    Io {
        /// The file that could not be read.
        path: std::path::PathBuf,
        /// What went wrong reading it.
        error: std::io::Error,
    },
    /// The certificate PEM held no certificates.
    #[cfg(feature = "server-tls")]
    NoCertificates {
        /// The file that was empty of them.
        path: std::path::PathBuf,
    },
    /// The key PEM held no private key.
    #[cfg(feature = "server-tls")]
    NoPrivateKey {
        /// The file that was empty of one.
        path: std::path::PathBuf,
    },
    /// The pair did not form a usable `rustls` configuration.
    #[cfg(feature = "server-tls")]
    Config(rustls::Error),
    /// The kernel's TLS offload could not be probed.
    #[cfg(all(feature = "server-tls", target_os = "linux"))]
    KernelProbe(std::io::Error),
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(not(feature = "server-tls"))]
            Self::FeatureMissing => write!(
                f,
                "TLS is configured, but this binary was built without skyzen's `server-tls` \
                 feature; add `features = [\"server-tls\"]` to the skyzen dependency"
            ),
            #[cfg(feature = "server-tls")]
            Self::NotTcp => write!(
                f,
                "TLS needs a TCP listener; a unix socket has no transport to encrypt \
                 (a fronting proxy terminates TLS for it instead)"
            ),
            #[cfg(feature = "server-tls")]
            Self::Io { path, error } => {
                write!(f, "could not read {}: {error}", path.display())
            }
            #[cfg(feature = "server-tls")]
            Self::NoCertificates { path } => {
                write!(f, "{} contains no PEM certificates", path.display())
            }
            #[cfg(feature = "server-tls")]
            Self::NoPrivateKey { path } => {
                write!(f, "{} contains no PEM private key", path.display())
            }
            #[cfg(feature = "server-tls")]
            Self::Config(error) => write!(f, "the TLS certificate/key pair is not usable: {error}"),
            #[cfg(all(feature = "server-tls", target_os = "linux"))]
            Self::KernelProbe(error) => {
                write!(f, "could not probe the kernel's TLS offload: {error}")
            }
        }
    }
}

impl std::error::Error for TlsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(feature = "server-tls")]
            Self::Io { error, .. } => Some(error),
            #[cfg(feature = "server-tls")]
            Self::Config(error) => Some(error),
            #[cfg(all(feature = "server-tls", target_os = "linux"))]
            Self::KernelProbe(error) => Some(error),
            _ => None,
        }
    }
}

/// The TLS acceptor shared by every connection of every thread.
#[cfg(feature = "server-tls")]
#[derive(Debug, Clone)]
pub(super) struct ServerTls(http_kit_tls::TlsAcceptor);

/// Without `server-tls` the type is uninhabited, which is what makes the `None` the launch path
/// hands the threads a fact rather than a promise.
#[cfg(not(feature = "server-tls"))]
#[derive(Debug, Clone)]
pub(super) enum ServerTls {}

/// How long a freshly accepted TLS connection may take to handshake, and an `h2` one to send its
/// preface, before it is dropped. Prevents idle clients from pinning per-connection tasks forever.
#[cfg(feature = "server-tls")]
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Turn the configured [`TlsFiles`] into the acceptor every connection shares, or refuse the
/// start when they cannot become one.
///
/// A non-TCP listener is refused here: a filesystem socket has no transport to encrypt, so flags
/// that produced `files` for it can only have been set by mistake. This runs once, on the
/// starting thread, before any serving thread exists, which is where the kernel is probed.
#[cfg(feature = "server-tls")]
pub(super) fn build(
    files: Option<&TlsFiles>,
    addr: &ListenAddr,
) -> Result<Option<ServerTls>, RuntimeFailure> {
    match files {
        None => Ok(None),
        Some(_) if !matches!(addr, ListenAddr::Tcp(_)) => {
            Err(RuntimeFailure::Tls(TlsError::NotTcp))
        }
        Some(files) => load_tls_acceptor(files)
            .map(|acceptor| Some(ServerTls(acceptor)))
            .map_err(RuntimeFailure::Tls),
    }
}

/// Without `server-tls` a configured pair refuses the start and names the feature to add; the
/// alternative is cleartext under settings that promised TLS.
#[cfg(not(feature = "server-tls"))]
pub(super) fn build(
    files: Option<&TlsFiles>,
    _addr: &ListenAddr,
) -> Result<Option<ServerTls>, RuntimeFailure> {
    match files {
        None => Ok(None),
        Some(_) => Err(RuntimeFailure::Tls(TlsError::FeatureMissing)),
    }
}

/// What reading the PEM file at `path` failed with.
#[cfg(feature = "server-tls")]
fn pem_error(path: &std::path::Path, error: rustls::pki_types::pem::Error) -> TlsError {
    use rustls::pki_types::pem::Error;

    let error = match error {
        Error::Io(error) => error,
        other => std::io::Error::new(std::io::ErrorKind::InvalidData, other),
    };
    TlsError::Io {
        path: path.to_owned(),
        error,
    }
}

/// Load `files` and build the shared acceptor, advertising `h2` and `http/1.1` on ALPN so real
/// HTTPS clients can negotiate HTTP/2. On Linux the acceptor hands each connection's records to
/// the kernel once its handshake completes, where the kernel takes the negotiated suite.
///
/// The aws-lc-rs provider is the one the workspace already converged on — the AWS SDK's
/// `default-https-client` uses it — and it is handed to the builder explicitly rather than
/// installed as a process default.
#[cfg(feature = "server-tls")]
fn load_tls_acceptor(files: &TlsFiles) -> Result<http_kit_tls::TlsAcceptor, TlsError> {
    use rustls::pki_types::{pem::PemObject as _, CertificateDer, PrivateKeyDer};

    let certs = CertificateDer::pem_file_iter(&files.cert)
        .map_err(|error| pem_error(&files.cert, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| pem_error(&files.cert, error))?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificates {
            path: files.cert.clone(),
        });
    }

    let key = match PrivateKeyDer::from_pem_file(&files.key) {
        Ok(key) => key,
        Err(rustls::pki_types::pem::Error::NoItemsFound) => {
            return Err(TlsError::NoPrivateKey {
                path: files.key.clone(),
            })
        }
        Err(error) => return Err(pem_error(&files.key, error)),
    };

    let mut config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::aws_lc_rs::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .map_err(TlsError::Config)?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(TlsError::Config)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = http_kit_tls::TlsAcceptor::new(std::sync::Arc::new(config));
    #[cfg(target_os = "linux")]
    let acceptor =
        acceptor.with_kernel_tls(http_kit_tls::KernelTls::probe().map_err(TlsError::KernelProbe)?);
    Ok(acceptor)
}

/// Serve TLS connections from `listener` until `shutdown`, then wait for the open ones.
///
/// Nothing is shared with the other threads but the acceptor: the handshake, the protocol choice
/// and the connection all run on the thread that accepted it, on a task of its own, so a client
/// that stalls its handshake holds up nobody else's accept. HTTP/1.1 is served by `server`.
#[cfg(feature = "server-tls")]
pub(super) async fn serve<L, E>(
    executor: Rc<Runtime>,
    listener: L,
    endpoint: E,
    shutdown: impl Future<Output = ()> + 'static,
    tls: ServerTls,
    max_connections: NonZeroUsize,
    http1: (HttpServer, H1),
) where
    L: Listener<Stream = <Compio as Io>::TcpStream, Addr = SocketAddr>,
    E: Endpoint + Clone + 'static,
{
    use core::{convert::Infallible, pin::pin};

    use executor_core::{LocalExecutor as _, Task as _};
    use futures_util::future::{select, Either};
    use http_kit_io::CopyingPoll;
    use skyzen_hyper::{serve_connection, IntoService, Prefixed, Protocol};
    use tracing::{debug, debug_span, error, warn, Instrument as _};

    // Nothing is ever sent on either channel. Dropping `stop` closes it, which every connection
    // watches to begin its graceful shutdown; each connection holds a clone of `live`, so
    // `drained` resolves once the last one has closed.
    let (stop, stopping) = async_channel::bounded::<Infallible>(1);
    let (live, drained) = async_channel::bounded::<Infallible>(1);
    let open = Rc::new(Cell::new(0_usize));

    let mut shutdown = pin!(shutdown);
    loop {
        let accept = pin!(listener.accept());
        let (stream, peer) = match select(accept, shutdown.as_mut()).await {
            Either::Left((Ok(accepted), _)) => accepted,
            Either::Left((Err(error), _)) => {
                error!("accept failed: {error}");
                continue;
            }
            Either::Right(((), _)) => break,
        };
        if open.get() >= max_connections.get() {
            warn!("connection limit reached; closing a connection");
            continue;
        }
        debug!("accepted connection from {peer}");

        open.set(open.get() + 1);
        let slot = Slot(Rc::clone(&open));
        let live = live.clone();
        let stopping = stopping.clone();
        let tls = tls.clone();
        let endpoint = endpoint.clone();
        let h1 = http1.1.clone();
        let connection_executor = Rc::clone(&executor);
        executor
            .spawn_local(
                async move {
                    let _slot = slot;
                    let Some(accepted) =
                        negotiate(stream, &tls, HANDSHAKE_TIMEOUT, HANDSHAKE_TIMEOUT).await
                    else {
                        return;
                    };
                    let stop = async move {
                        // Resolves with an error once `stop` is dropped.
                        let _ = stopping.recv().await;
                    };
                    let service = |endpoint: E| IntoService::new(endpoint).with_peer_addr(peer);
                    match (accepted, http1.0) {
                        (Accepted::Http1(stream), HttpServer::H1) => {
                            h1.serve_connection(Compio, stream, Some(peer), endpoint, stop, live)
                                .await;
                        }
                        (Accepted::Http1(stream), HttpServer::Hyper) => {
                            let _live = live;
                            let stream = Prefixed::new(CopyingPoll::new(stream), Vec::new());
                            serve_connection(
                                stream,
                                Protocol::Http1,
                                service(endpoint),
                                connection_executor,
                                stop,
                            )
                            .await;
                        }
                        (Accepted::Http2(stream), _) => {
                            let _live = live;
                            serve_connection(
                                stream,
                                Protocol::Http2,
                                service(endpoint),
                                connection_executor,
                                stop,
                            )
                            .await;
                        }
                    }
                }
                .instrument(debug_span!("tls_connection", %peer)),
            )
            .detach();
    }

    drop(stop);
    drop(live);
    let _ = drained.recv().await;
}

/// Without `server-tls` there is no [`ServerTls`] to hold, so this cannot be called.
#[cfg(not(feature = "server-tls"))]
pub(super) async fn serve<L, E>(
    _executor: Rc<Runtime>,
    _listener: L,
    _endpoint: E,
    _shutdown: impl Future<Output = ()> + 'static,
    tls: ServerTls,
    _max_connections: NonZeroUsize,
    _http1: (super::HttpServer, skyzen_h1::H1),
) where
    L: Listener<Stream = <Compio as Io>::TcpStream, Addr = SocketAddr>,
    E: Endpoint + Clone + 'static,
{
    match tls {}
}

/// Counts a connection against the limit for as long as it lives.
#[cfg(feature = "server-tls")]
struct Slot(Rc<Cell<usize>>);

#[cfg(feature = "server-tls")]
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// A connection that has finished its TLS handshake, by the protocol ALPN chose for it.
#[cfg(feature = "server-tls")]
enum Accepted<S: http_kit_tls::Transport> {
    /// HTTP/1.1, on the TLS stream itself.
    Http1(http_kit_tls::TlsStream<S, http_kit_tls::Server>),
    /// HTTP/2, whose preface has been read and is replayed ahead of the stream.
    Http2(
        skyzen_hyper::Prefixed<
            http_kit_io::CopyingPoll<http_kit_tls::TlsStream<S, http_kit_tls::Server>>,
        >,
    ),
}

/// Run the TLS handshake under `handshake` and work out which HTTP version the connection speaks,
/// waiting at most `preface` for an `h2` client's preface.
///
/// The connection's TLS mode, version, suite and ALPN outcome are recorded as a debug event in
/// the connection's span.
///
/// `None` means the connection is already dead — a failed handshake or an `h2` client that never
/// sent its preface — having been logged here rather than by the caller.
#[cfg(feature = "server-tls")]
async fn negotiate<S>(
    stream: S,
    tls: &ServerTls,
    handshake: std::time::Duration,
    preface: std::time::Duration,
) -> Option<Accepted<S>>
where
    S: http_kit_tls::Transport,
{
    use http_kit_io::CopyingPoll;
    use skyzen_hyper::{sniff_protocol, Protocol};
    use tracing::debug;

    let acceptor = tls.0.clone().with_handshake_timeout(Some(handshake));
    let stream = match acceptor.accept(stream, &Compio).await {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            debug!("timed out in TLS handshake");
            return None;
        }
        Err(error) => {
            // TCP probes that connect and close, and plaintext clients knocking on an HTTPS
            // port, are routine noise on a public listener — debug, not error.
            debug!("TLS handshake failed: {error}");
            return None;
        }
    };

    let negotiated = stream.negotiated();
    debug!(
        mode = ?negotiated.mode(),
        version = ?negotiated.protocol_version(),
        suite = ?negotiated.cipher_suite().suite(),
        alpn = ?negotiated.alpn_protocol().map(String::from_utf8_lossy),
        "TLS handshake complete"
    );

    // RFC 9113 §3.3: over TLS the protocol comes from ALPN alone — an `h2` promise still owes the
    // real preface inside the bound (without it an idle client pins the connection task forever),
    // and any other negotiated outcome is served HTTP/1.1 whatever bytes the client sends. There
    // is no h2c-by-sniffing on a TLS connection.
    match stream.alpn_protocol() {
        Some(b"h2") => match sniff_protocol(CopyingPoll::new(stream), preface).await {
            Ok((prefixed, Protocol::Http2)) => Some(Accepted::Http2(prefixed)),
            Ok(_) => {
                debug!("ALPN negotiated h2 but the connection does not open with the preface");
                None
            }
            Err(error) => {
                debug!("failed to read the connection preface: {error}");
                None
            }
        },
        _ => Some(Accepted::Http1(stream)),
    }
}

#[cfg(all(test, feature = "server-tls"))]
mod tests {
    use core::convert::Infallible;
    use std::{rc::Rc, sync::Arc};

    use executor_core::compio::Runtime;
    use http_kit::{Body, Response};
    use http_kit_io::{compio::Compio, BufResult, Io as _, Listener as _, Read as _};
    use http_kit_tls::{rustls, TlsConnector};
    use skyzen_h1::H1;
    use skyzen_hyper::HTTP2_PREFACE;

    use super::{
        load_tls_acceptor, negotiate, serve, Accepted, ServerTls, TlsError, HANDSHAKE_TIMEOUT,
    };
    use crate::{
        runtime::native::{HttpServer, TlsFiles},
        Endpoint,
    };

    /// The preface wait of the test whose client completes its handshake and then goes silent.
    /// Only that wait is shortened: every handshake in these tests runs under the production
    /// [`HANDSHAKE_TIMEOUT`], so no test depends on how fast a loaded debug build handshakes.
    const SHORT_PREFACE: core::time::Duration = core::time::Duration::from_millis(250);

    /// Chinese, Japanese and Korean, so a body that is not ASCII crosses the records intact.
    const CJK: &str = "你好，世界。こんにちは、世界。안녕하세요, 세계.";

    /// A self-signed leaf + key as PEM files in a fresh temporary directory.
    /// The leaf's DER bytes come back too, so the client side can pin it as its root.
    fn tls_material() -> (tempfile::TempDir, TlsFiles, Vec<u8>) {
        let certified =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("rcgen");
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, certified.cert.pem()).expect("write cert");
        std::fs::write(&key, certified.signing_key.serialize_pem()).expect("write key");
        (
            dir,
            TlsFiles { cert, key },
            certified.cert.der().as_ref().to_vec(),
        )
    }

    /// A TLS client verifying against the generated chain and offering one ALPN
    /// protocol — the same real handshake rustls runs for any HTTPS client.
    fn tls_connector(der: &[u8], alpn: &[u8]) -> TlsConnector {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(der.to_vec()))
            .expect("the generated cert is a valid root");
        let mut config = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![alpn.to_vec()];
        TlsConnector::new(Arc::new(config))
    }

    fn server_name() -> rustls::pki_types::ServerName<'static> {
        rustls::pki_types::ServerName::try_from("localhost").expect("localhost is a DNS name")
    }

    fn acceptor() -> (tempfile::TempDir, ServerTls, Vec<u8>) {
        let (dir, files, der) = tls_material();
        let tls = ServerTls(load_tls_acceptor(&files).expect("the generated pair loads"));
        (dir, tls, der)
    }

    /// Run `test` on a compio runtime, handing it the runtime to spawn servers on.
    fn on_compio<F: std::future::Future>(test: impl FnOnce(Rc<Runtime>) -> F) -> F::Output {
        let runtime = Rc::new(Runtime::new().expect("compio runtime"));
        let future = test(Rc::clone(&runtime));
        runtime.block_on(future)
    }

    /// Connect to `addr` and complete a TLS handshake offering `alpn`.
    async fn tls_client(
        addr: std::net::SocketAddr,
        der: &[u8],
        alpn: &[u8],
    ) -> http_kit_tls::TlsStream<<Compio as http_kit_io::Io>::TcpStream, http_kit_tls::Client> {
        let stream = Compio.connect(addr).await.expect("connect");
        tls_connector(der, alpn)
            .connect(server_name(), stream, &Compio)
            .await
            .expect("client handshake")
    }

    async fn write_all(stream: &mut (impl http_kit_io::Write + ?Sized), bytes: Vec<u8>) {
        let BufResult(written, _) = stream.write_all(bytes).await;
        written.expect("write");
    }

    /// Read until `stream` has produced at least `wanted` bytes or ends.
    async fn read_at_least(stream: &mut impl http_kit_io::Read, wanted: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = Vec::with_capacity(16 * 1024);
        while out.len() < wanted {
            let BufResult(read, filled) = stream.read(buf).await;
            if read.expect("read") == 0 {
                break;
            }
            out.extend_from_slice(&filled);
            buf = filled;
        }
        out
    }

    /// Send a `GET` over `stream` and read the response, its head and its `Content-Length` body.
    async fn get(stream: &mut (impl http_kit_io::Read + http_kit_io::Write)) -> (String, Vec<u8>) {
        write_all(
            stream,
            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        let mut response = Vec::new();
        let (head_len, length) = loop {
            let more = read_at_least(stream, 1).await;
            assert!(!more.is_empty(), "the response ended inside its head");
            response.extend_from_slice(&more);
            if let Some(end) = response.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&response[..end]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .map(|value| value.trim().parse::<usize>().expect("a numeric length"))
                    .expect("the response has a content-length");
                break (end + 4, length);
            }
        };
        while response.len() < head_len + length {
            let more = read_at_least(stream, 1).await;
            assert!(!more.is_empty(), "the response ended inside its body");
            response.extend_from_slice(&more);
        }
        let head = String::from_utf8_lossy(&response[..head_len]).into_owned();
        (head, response[head_len..head_len + length].to_vec())
    }

    #[test]
    fn tls_acceptor_loads_a_real_pem_pair() {
        let (_dir, files, _der) = tls_material();
        load_tls_acceptor(&files).expect("a generated pair builds an acceptor");
    }

    #[test]
    fn tls_acceptor_names_the_file_it_cannot_read() {
        let files = TlsFiles {
            cert: "/no/such/cert.pem".into(),
            key: "/no/such/key.pem".into(),
        };
        match load_tls_acceptor(&files) {
            Err(TlsError::Io { path, .. }) => {
                assert_eq!(path, std::path::Path::new("/no/such/cert.pem"));
            }
            _ => panic!("expected an Io error naming the cert path"),
        }
    }

    #[test]
    fn tls_acceptor_rejects_a_pem_without_certificates() {
        let (_dir, files, _der) = tls_material();
        std::fs::write(&files.cert, "definitely not a PEM file\n").expect("write");
        match load_tls_acceptor(&files) {
            Err(TlsError::NoCertificates { path }) => {
                assert_eq!(path, files.cert);
            }
            _ => panic!("expected NoCertificates"),
        }
    }

    #[test]
    fn tls_acceptor_rejects_a_pem_without_a_key() {
        let (_dir, files, _der) = tls_material();
        std::fs::write(&files.key, "definitely not a PEM file\n").expect("write");
        match load_tls_acceptor(&files) {
            Err(TlsError::NoPrivateKey { path }) => {
                assert_eq!(path, files.key);
            }
            _ => panic!("expected NoPrivateKey"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn tls_on_a_unix_listener_is_refused() {
        let files = TlsFiles {
            cert: "cert.pem".into(),
            key: "key.pem".into(),
        };
        let addr = crate::runtime::native::ListenAddr::Unix("/tmp/skyzen-tls-test.sock".into());
        match super::build(Some(&files), &addr) {
            Err(crate::runtime::native::RuntimeFailure::Tls(TlsError::NotTcp)) => {}
            other => panic!("expected the unix listener to refuse TLS, got {other:?}"),
        }
    }

    /// The endpoint the TLS tests serve through the real `IntoService` machinery.
    #[derive(Clone)]
    struct TlsEndpoint;

    impl Endpoint for TlsEndpoint {
        type Error = Infallible;

        fn respond(
            &mut self,
            _request: &mut crate::Request,
        ) -> impl std::future::Future<Output = Result<crate::Response, Self::Error>> {
            std::future::ready(Ok(Response::new(Body::from_bytes(CJK))))
        }
    }

    /// Start the whole per-thread pipeline on `runtime`, serving `endpoint` with `server`, and
    /// return the address plus the handle that stops it.
    fn start<E: Endpoint + Clone + 'static>(
        runtime: &Rc<Runtime>,
        tls: ServerTls,
        server: HttpServer,
        endpoint: E,
    ) -> (
        std::net::SocketAddr,
        async_channel::Sender<Infallible>,
        impl std::future::Future<Output = ()>,
    ) {
        let listener = Compio
            .bind_tcp("127.0.0.1:0".parse().unwrap())
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let (stop, stopping) = async_channel::bounded::<Infallible>(1);
        let task = executor_core::LocalExecutor::spawn_local(
            &**runtime,
            serve(
                Rc::clone(runtime),
                listener,
                endpoint,
                async move {
                    let _ = stopping.recv().await;
                },
                tls,
                8.try_into().unwrap(),
                (server, H1::default()),
            ),
        );
        (addr, stop, task)
    }

    /// A real loopback handshake through the whole per-thread pipeline, for each server HTTP/1.1
    /// can be given: ALPN `http/1.1` gets an actual response over the decrypted stream, with a
    /// body of Chinese, Japanese and Korean text.
    #[test]
    fn https_http1_is_served_by_either_server() {
        for server in [HttpServer::H1, HttpServer::Hyper] {
            let (_dir, tls, der) = acceptor();
            on_compio(|runtime| async move {
                let (addr, stop, task) = start(&runtime, tls, server, TlsEndpoint);

                let mut client = tls_client(addr, &der, b"http/1.1").await;
                assert_eq!(client.alpn_protocol(), Some(b"http/1.1".as_slice()));
                let (head, body) = get(&mut client).await;
                assert!(head.starts_with("HTTP/1.1 200"), "{server:?}: {head}");
                assert_eq!(
                    String::from_utf8(body).expect("UTF-8 body"),
                    CJK,
                    "{server:?}"
                );

                drop(stop);
                task.await;
            });
        }
    }

    /// ALPN `h2` is hyper's HTTP/2: after the client preface and an empty `SETTINGS` frame the
    /// server answers with its own `SETTINGS` frame (type 4), whichever server HTTP/1.1 uses.
    #[test]
    fn https_h2_is_negotiated_and_served() {
        for server in [HttpServer::H1, HttpServer::Hyper] {
            let (_dir, tls, der) = acceptor();
            on_compio(|runtime| async move {
                let (addr, stop, task) = start(&runtime, tls, server, TlsEndpoint);

                let mut client = tls_client(addr, &der, b"h2").await;
                assert_eq!(client.alpn_protocol(), Some(b"h2".as_slice()));
                let mut hello = HTTP2_PREFACE.to_vec();
                hello.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
                write_all(&mut client, hello).await;
                let frame = read_at_least(&mut client, 9).await;
                assert!(
                    frame.len() >= 9,
                    "{server:?}: no frame header, got {frame:?}"
                );
                assert_eq!(frame[3], 4, "{server:?}: expected a SETTINGS frame");

                drop(client);
                drop(stop);
                task.await;
            });
        }
    }

    /// An `h2` client still owes the connection its preface inside the same bound a cleartext
    /// one gets: complete the handshake, then send nothing — the server must drop the
    /// connection rather than wait forever.
    #[test]
    fn an_h2_client_that_never_sends_a_preface_is_dropped() {
        let (_dir, tls, der) = acceptor();
        on_compio(|_| async move {
            let listener = Compio
                .bind_tcp("127.0.0.1:0".parse().unwrap())
                .expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");

            let serve = async {
                let (stream, _peer) = listener.accept().await.expect("accept");
                negotiate(stream, &tls, HANDSHAKE_TIMEOUT, SHORT_PREFACE)
                    .await
                    .is_none()
            };
            let client = async {
                let mut tls = tls_client(addr, &der, b"h2").await;
                // Handshake done — now go silent. The server's deadline closes this.
                let BufResult(read, _) = tls.read(vec![0_u8; 8]).await;
                read
            };
            // Boxed: on Linux the stream carries kernel TLS state, which makes the join large.
            let (dropped, read) = Box::pin(futures_util::future::join(serve, client)).await;
            assert!(
                dropped,
                "a silent h2 connection must be dropped, not served"
            );
            assert!(
                matches!(read, Ok(0) | Err(_)),
                "the client must observe the close, got {read:?}"
            );
        });
    }

    /// RFC 9113 §3.3: over TLS the protocol is ALPN's, not the bytes'. An `http/1.1` client that
    /// sends the HTTP/2 preface anyway must still be served HTTP/1 — cleartext keeps its sniff,
    /// TLS does not get one.
    #[test]
    fn an_h1_alpn_client_sending_the_h2_preface_stays_h1() {
        let (_dir, tls, der) = acceptor();
        on_compio(|_| async move {
            let listener = Compio
                .bind_tcp("127.0.0.1:0".parse().unwrap())
                .expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");

            let serve = async {
                let (stream, _peer) = listener.accept().await.expect("accept");
                negotiate(stream, &tls, HANDSHAKE_TIMEOUT, HANDSHAKE_TIMEOUT).await
            };
            let request = async {
                let mut tls = tls_client(addr, &der, b"http/1.1").await;
                write_all(&mut tls, HTTP2_PREFACE.to_vec()).await;
                tls
            };
            // Boxed: on Linux the stream carries kernel TLS state, which makes the join large.
            let (accepted, _tls) = Box::pin(futures_util::future::join(serve, request)).await;
            assert!(
                matches!(accepted, Some(Accepted::Http1(_))),
                "an http/1.1 ALPN must never serve h2"
            );
        });
    }

    /// A TCP probe that connects and closes, and a plaintext client on the HTTPS port, are
    /// refused at the handshake without taking anything else down.
    #[test]
    fn a_failed_handshake_ends_the_connection_quietly() {
        let (_dir, tls, _der) = acceptor();
        on_compio(|_| async move {
            let listener = Compio
                .bind_tcp("127.0.0.1:0".parse().unwrap())
                .expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");

            let serve = async {
                let (stream, _peer) = listener.accept().await.expect("accept");
                negotiate(stream, &tls, HANDSHAKE_TIMEOUT, HANDSHAKE_TIMEOUT)
                    .await
                    .is_none()
            };
            let client = async {
                let mut stream = Compio.connect(addr).await.expect("connect");
                write_all(&mut stream, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec()).await;
                stream
            };
            let (refused, _stream) = futures_util::future::join(serve, client).await;
            assert!(refused, "a plaintext client must not complete a handshake");
        });
    }

    /// On Linux every accepted connection moves to kernel TLS, and a file region goes out through
    /// it intact. The mode is read from the handshake event the runtime records in the
    /// connection's span.
    #[test]
    #[cfg(target_os = "linux")]
    fn linux_connections_use_kernel_tls_and_serve_file_regions() {
        use std::sync::Mutex;

        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log lock").extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
            type Writer = Self;

            fn make_writer(&'a self) -> Self {
                self.clone()
            }
        }

        /// Answers every request with the whole of a file, as a file-region body.
        #[derive(Clone)]
        struct FileEndpoint(std::path::PathBuf, u64);

        impl Endpoint for FileEndpoint {
            type Error = Infallible;

            fn respond(
                &mut self,
                _request: &mut crate::Request,
            ) -> impl std::future::Future<Output = Result<crate::Response, Self::Error>>
            {
                let file = std::fs::File::open(&self.0).expect("open the file to serve");
                std::future::ready(Ok(Response::new(Body::from_file_region(
                    http_kit::FileRegion::new(file, 0, self.1),
                ))))
            }
        }

        let payload: Vec<u8> = (0..20_000)
            .flat_map(|line| format!("{line:06} {CJK}\n").into_bytes())
            .collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("payload.txt");
        std::fs::write(&path, &payload).expect("write the payload");

        let log = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(log.clone())
            .finish();
        let (_dir, tls, der) = acceptor();
        tracing::subscriber::with_default(subscriber, || {
            on_compio(|runtime| async move {
                let endpoint = FileEndpoint(path, payload.len() as u64);
                let (addr, stop, task) = start(&runtime, tls, HttpServer::H1, endpoint);

                let mut client = tls_client(addr, &der, b"http/1.1").await;
                let (head, body) = get(&mut client).await;
                assert!(head.starts_with("HTTP/1.1 200"), "{head}");
                assert!(body == payload, "the file region arrived changed");

                drop(stop);
                task.await;
            });
        });

        let log = String::from_utf8(log.0.lock().expect("log lock").clone()).expect("UTF-8 log");
        assert!(
            log.contains("TLS handshake complete") && log.contains("mode=Kernel"),
            "the connection did not move to kernel TLS:\n{log}"
        );
    }
}

#[cfg(all(test, not(feature = "server-tls")))]
mod tests {
    use super::{TlsError, TlsFiles};
    use crate::runtime::native::{ListenAddr, RuntimeFailure};

    /// Without `server-tls` a configured pair is refused at startup and names the feature — it
    /// must never degrade to cleartext.
    #[test]
    fn tls_config_without_the_feature_is_refused() {
        let files = TlsFiles {
            cert: "cert.pem".into(),
            key: "key.pem".into(),
        };
        let addr = ListenAddr::Tcp("127.0.0.1:0".parse().unwrap());
        match super::build(Some(&files), &addr) {
            Err(RuntimeFailure::Tls(TlsError::FeatureMissing)) => {}
            other => panic!("expected the missing feature to be refused, got {other:?}"),
        }
    }
}
