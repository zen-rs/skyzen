//! TLS on the per-core threads.
//!
//! This is the interim path, until TLS runs on owned-buffer streams with kernel offload
//! (zen-rs/skyzen#182): `futures-rustls` is driven over the compio stream's poll-style capability,
//! which copies every read and write through a buffer of its own, and ALPN picks the protocol
//! hyper then speaks on the decrypted stream — HTTP/2 for `h2`, HTTP/1.1 for anything else.
//! Skyzen's own `skyzen-h1` serves cleartext only, so a TLS listener is always hyper's.
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
    Config(futures_rustls::rustls::Error),
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
            _ => None,
        }
    }
}

/// The TLS acceptor shared by every connection of every thread.
#[cfg(feature = "server-tls")]
#[derive(Clone)]
pub(super) struct ServerTls(futures_rustls::TlsAcceptor);

#[cfg(feature = "server-tls")]
impl std::fmt::Debug for ServerTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ServerTls").finish_non_exhaustive()
    }
}

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
/// that produced `files` for it can only have been set by mistake.
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

/// Load `files` and build the shared acceptor, advertising `h2` and `http/1.1` on ALPN so real
/// HTTPS clients can negotiate HTTP/2.
///
/// The aws-lc-rs provider is the one the workspace already converged on — the AWS SDK's
/// `default-https-client` uses it — and it is handed to the builder explicitly rather than
/// installed as a process default.
#[cfg(feature = "server-tls")]
fn load_tls_acceptor(files: &TlsFiles) -> Result<futures_rustls::TlsAcceptor, TlsError> {
    use futures_rustls::rustls;

    let cert_file = std::fs::File::open(&files.cert).map_err(|error| TlsError::Io {
        path: files.cert.clone(),
        error,
    })?;
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(cert_file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsError::Io {
            path: files.cert.clone(),
            error,
        })?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificates {
            path: files.cert.clone(),
        });
    }

    let key_file = std::fs::File::open(&files.key).map_err(|error| TlsError::Io {
        path: files.key.clone(),
        error,
    })?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_file))
        .map_err(|error| TlsError::Io {
            path: files.key.clone(),
            error,
        })?
        .ok_or_else(|| TlsError::NoPrivateKey {
            path: files.key.clone(),
        })?;

    let mut config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::aws_lc_rs::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .map_err(TlsError::Config)?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(TlsError::Config)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(futures_rustls::TlsAcceptor::from(std::sync::Arc::new(
        config,
    )))
}

/// Serve TLS connections from `listener` until `shutdown`, then wait for the open ones.
///
/// Nothing is shared with the other threads but the acceptor: the handshake, the protocol choice
/// and the connection all run on the thread that accepted it.
#[cfg(feature = "server-tls")]
pub(super) async fn serve<L, E>(
    executor: Rc<Runtime>,
    listener: L,
    endpoint: E,
    shutdown: impl Future<Output = ()> + 'static,
    tls: ServerTls,
    max_connections: NonZeroUsize,
) where
    L: Listener<Stream = <Compio as Io>::TcpStream, Addr = SocketAddr>,
    E: Endpoint + Clone + 'static,
{
    use core::{convert::Infallible, pin::pin};

    use executor_core::{LocalExecutor as _, Task as _};
    use futures_util::future::{select, Either};
    use http_kit_io::IntoPoll as _;
    use skyzen_hyper::{serve_connection, IntoService};
    use tracing::{debug, error, warn};

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
        let service = IntoService::new(endpoint.clone()).with_peer_addr(peer);
        let connection_executor = Rc::clone(&executor);
        executor
            .spawn_local(async move {
                let _slot = slot;
                let _live = live;
                let Some((stream, protocol)) = negotiate(
                    stream.into_poll(),
                    &tls,
                    HANDSHAKE_TIMEOUT,
                    HANDSHAKE_TIMEOUT,
                )
                .await
                else {
                    return;
                };
                serve_connection(stream, protocol, service, connection_executor, async move {
                    // Resolves with an error once `stop` is dropped.
                    let _ = stopping.recv().await;
                })
                .await;
            })
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

/// Run the TLS handshake under `handshake` and work out which HTTP version the connection speaks,
/// waiting at most `preface` for an `h2` client's preface, returning the decrypted stream and its
/// protocol.
///
/// `None` means the connection is already dead — a failed handshake or an `h2` client that never
/// sent its preface — having been logged here rather than by the caller.
#[cfg(feature = "server-tls")]
async fn negotiate<C>(
    stream: C,
    tls: &ServerTls,
    handshake: std::time::Duration,
    preface: std::time::Duration,
) -> Option<(
    skyzen_hyper::Prefixed<futures_rustls::server::TlsStream<C>>,
    skyzen_hyper::Protocol,
)>
where
    C: http_kit::utils::AsyncRead + http_kit::utils::AsyncWrite + Unpin,
{
    use core::pin::pin;

    use futures_util::future::{select, Either};
    use http_kit_io::Timer as _;
    use skyzen_hyper::{sniff_protocol, Prefixed, Protocol};
    use tracing::{debug, error};

    let accepting = pin!(tls.0.accept(stream));
    let stream = match select(accepting, pin!(Compio.sleep(handshake))).await {
        Either::Left((Ok(stream), _)) => stream,
        Either::Left((Err(error), _)) => {
            // TCP probes that connect and close, and plaintext clients knocking on an HTTPS
            // port, are routine noise on a public listener — debug, not error.
            debug!("TLS handshake failed: {error}");
            return None;
        }
        Either::Right(((), _)) => {
            debug!("timed out in TLS handshake");
            return None;
        }
    };

    // RFC 9113 §3.3: over TLS the protocol comes from ALPN alone — an `h2` promise still owes the
    // real preface inside the bound (without it an idle client pins the connection task forever),
    // and any other negotiated outcome is served HTTP/1.1 whatever bytes the client sends. There
    // is no h2c-by-sniffing on a TLS connection.
    match stream.get_ref().1.alpn_protocol() {
        Some(b"h2") => match sniff_protocol(stream, preface).await {
            Ok((prefixed, Protocol::Http2)) => Some((prefixed, Protocol::Http2)),
            Ok(_) => {
                debug!("ALPN negotiated h2 but the connection does not open with the preface");
                None
            }
            Err(error) => {
                error!("Failed to read connection preface: {error}");
                None
            }
        },
        _ => Some((Prefixed::new(stream, Vec::new()), Protocol::Http1)),
    }
}

#[cfg(all(test, feature = "server-tls"))]
mod tests {
    use core::convert::Infallible;
    use std::{rc::Rc, sync::Arc};

    use executor_core::compio::Runtime;
    use futures_rustls::rustls;
    use http_kit::utils::{AsyncReadExt as _, AsyncWriteExt as _};
    use http_kit_io::{compio::Compio, IntoPoll as _, Io as _, Listener as _};
    use skyzen_hyper::{Protocol, HTTP2_PREFACE};

    use super::{load_tls_acceptor, negotiate, serve, ServerTls, TlsError, HANDSHAKE_TIMEOUT};
    use crate::{runtime::native::TlsFiles, Endpoint};

    /// The preface wait of the test whose client completes its handshake and then goes silent.
    /// Only that wait is shortened: every handshake in these tests runs under the production
    /// [`HANDSHAKE_TIMEOUT`], so no test depends on how fast a loaded debug build handshakes.
    const SHORT_PREFACE: core::time::Duration = core::time::Duration::from_millis(250);

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
    fn tls_connector(der: &[u8], alpn: &[u8]) -> futures_rustls::TlsConnector {
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
        futures_rustls::TlsConnector::from(Arc::new(config))
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
            std::future::ready(Ok(crate::Response::new(crate::Body::from_bytes("tls-ok"))))
        }
    }

    /// A real loopback handshake through the whole per-thread pipeline: accept, handshake, ALPN,
    /// hyper. An `h2` client sees `h2` negotiated, and an `http/1.1` client gets an actual HTTP
    /// response over the decrypted stream.
    #[test]
    fn tls_connection_serves_http1_and_negotiates_h2() {
        let (_dir, tls, der) = acceptor();
        on_compio(|runtime| async move {
            let listener = Compio
                .bind_tcp("127.0.0.1:0".parse().unwrap())
                .expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");
            let (stop, stopping) = async_channel::bounded::<Infallible>(1);
            let server = executor_core::LocalExecutor::spawn_local(
                &*runtime,
                serve(
                    Rc::clone(&runtime),
                    listener,
                    TlsEndpoint,
                    async move {
                        let _ = stopping.recv().await;
                    },
                    tls,
                    8.try_into().unwrap(),
                ),
            );

            // An h2 client: ALPN decides the protocol, so the client must see `h2`.
            let stream = Compio.connect(addr).await.expect("connect");
            let client = tls_connector(&der, b"h2")
                .connect(server_name(), stream.into_poll())
                .await
                .expect("client handshake");
            assert_eq!(client.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
            drop(client);

            // An http/1.1 client is served a real response.
            let stream = Compio.connect(addr).await.expect("connect");
            let mut tls = tls_connector(&der, b"http/1.1")
                .connect(server_name(), stream.into_poll())
                .await
                .expect("client handshake");
            tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .expect("write request");
            let mut response = Vec::new();
            tls.read_to_end(&mut response).await.expect("read response");
            let response = String::from_utf8(response).expect("response is HTTP text");
            assert!(
                response.starts_with("HTTP/1.1 200"),
                "expected a 200 status line, got: {response}"
            );
            assert!(
                response.contains("tls-ok"),
                "expected the body, got: {response}"
            );

            drop(stop);
            server.await;
        });
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
                negotiate(stream.into_poll(), &tls, HANDSHAKE_TIMEOUT, SHORT_PREFACE)
                    .await
                    .is_none()
            };
            let client = async {
                let stream = Compio.connect(addr).await.expect("connect");
                let mut tls = tls_connector(&der, b"h2")
                    .connect(server_name(), stream.into_poll())
                    .await
                    .expect("client handshake");
                // Handshake done — now go silent. The server's deadline closes this.
                let mut buf = [0u8; 8];
                tls.read(&mut buf).await
            };
            let (dropped, read) = futures_util::future::join(serve, client).await;
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
                negotiate(
                    stream.into_poll(),
                    &tls,
                    HANDSHAKE_TIMEOUT,
                    HANDSHAKE_TIMEOUT,
                )
                .await
            };
            let request = async {
                let stream = Compio.connect(addr).await.expect("connect");
                let mut tls = tls_connector(&der, b"http/1.1")
                    .connect(server_name(), stream.into_poll())
                    .await
                    .expect("client handshake");
                tls.write_all(HTTP2_PREFACE).await.expect("write preface");
                tls
            };
            let (prepared, _tls) = futures_util::future::join(serve, request).await;
            let (_stream, protocol) = prepared.expect("the connection is still served");
            assert_eq!(
                protocol,
                Protocol::Http1,
                "an http/1.1 ALPN must never serve h2"
            );
        });
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
