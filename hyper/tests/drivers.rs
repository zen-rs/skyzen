//! `skyzen-hyper` on every `http-kit-io` driver, completion-based `compio` included: HTTP/1.1,
//! HTTP/2 over cleartext (prior knowledge) and over TLS with ALPN, an upgrade handed to its
//! session, and — beside the TCP side — HTTP/3 on UDP, announced by `Alt-Svc`.
//!
//! The endpoint holds an `Rc`, so nothing the server drives is `Send`; each scenario runs on a
//! thread of its own, on the driver under test, and the clients run on `smol` in the test thread.

use std::{
    cell::Cell, convert::Infallible, fs::File, future::Future, io::Write as _, net::SocketAddr,
    path::PathBuf, pin::pin, rc::Rc, sync::Arc, thread,
};

use async_channel::Receiver;
use executor_core::{LocalExecutor, Task};
use futures_lite::future::{yield_now, zip};
use futures_rustls::{
    rustls::{self, pki_types::ServerName},
    TlsAcceptor, TlsConnector,
};
use futures_util::future::{select, Either};
use http_body_util::{BodyExt, Empty};
use http_kit::{
    error::BoxHttpError, header, Body, FileRegion, Request, Response, StatusCode, Version,
};
use http_kit_io::{async_io::AsyncIo, compio::Compio, tokio::Tokio, IntoPoll, Io, Listener};
use skyzen_core::{
    error_response,
    upgrade::{OnUpgrade, UpgradeAvailable},
    Endpoint, Server,
};
use skyzen_hyper::{serve_connection, AltSvc, Hyper, IntoService, Protocol};
use smol::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use smol_hyper::rt::FuturesIo;

/// Runs the h2 client's stream tasks on smol's global executor.
#[derive(Clone, Copy)]
struct SmolExecutor;

impl<F> hyper::rt::Executor<F> for SmolExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        smol::spawn(future).detach();
    }
}

/// Chinese, Japanese and Korean, so every body crosses the wire as multi-byte text.
const CJK: &str = "你好，世界。こんにちは、世界。안녕하세요, 세계.";
/// Bytes of the served file before the region the endpoint sends.
const FILE_PREFIX: &str = "XYZ";

// ---------------------------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------------------------

/// Answers on the thread that built it, counting in an `Rc<Cell<_>>`.
#[derive(Clone)]
struct App {
    served: Rc<Cell<u32>>,
    file: Rc<PathBuf>,
}

impl App {
    fn new(file: PathBuf) -> Self {
        Self {
            served: Rc::default(),
            file: Rc::new(file),
        }
    }
}

impl Endpoint for App {
    type Error = BoxHttpError;

    fn respond(
        &mut self,
        request: &mut Request,
    ) -> impl Future<Output = Result<Response, Self::Error>> {
        let served = Rc::clone(&self.served);
        let file = Rc::clone(&self.file);
        let path = request.uri().path().to_owned();
        let version = request.version();
        let upgradable = request.extensions().get::<UpgradeAvailable>().is_some();
        async move {
            // Held across an await on purpose: the future is `!Send`.
            yield_now().await;
            served.set(served.get() + 1);
            Ok(match path.as_str() {
                "/hello" => Response::new(Body::from(format!("{version:?} {CJK}"))),
                "/count" => Response::new(Body::from(served.get().to_string())),
                "/upgradable" => Response::new(Body::from(upgradable.to_string())),
                "/file" => {
                    let file = File::open(&*file).expect("open the served file");
                    let region = FileRegion::new(file, FILE_PREFIX.len() as u64, CJK.len() as u64);
                    Response::new(Body::from_file_region(region))
                }
                "/echo" => {
                    let mut response = Response::new(Body::empty());
                    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                    response
                        .headers_mut()
                        .insert(header::UPGRADE, header::HeaderValue::from_static("echo"));
                    response.headers_mut().insert(
                        header::CONNECTION,
                        header::HeaderValue::from_static("upgrade"),
                    );
                    response
                        .extensions_mut()
                        .insert(OnUpgrade::new(|mut io| async move {
                            io.write_all(b"ready").await.expect("greet");
                            let mut buf = [0u8; 4];
                            io.read_exact(&mut buf).await.expect("read after upgrade");
                            io.write_all(&buf).await.expect("echo");
                            io.flush().await.expect("flush");
                        }));
                    response
                }
                _ => Response::new(Body::empty()),
            })
        }
    }
}

/// A file whose bytes `/file` serves from [`FILE_PREFIX`] on.
fn served_file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("served.txt");
    let mut file = File::create(&path).expect("create the served file");
    write!(file, "{FILE_PREFIX}{CJK}tail").expect("write the served file");
    (dir, path)
}

// ---------------------------------------------------------------------------------------------
// Running a scenario on a driver
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Driver {
    AsyncIo,
    Tokio,
    Compio,
}

/// What a server thread does, generic over the driver it runs on.
trait Scenario: Send + 'static {
    async fn run<I: Io, Ex: LocalExecutor + 'static>(self, io: I, executor: Rc<Ex>);
}

/// Run `scenario` on a thread of its own, on `driver`'s runtime.
fn start<S: Scenario>(driver: Driver, scenario: S) -> thread::JoinHandle<()> {
    thread::spawn(move || match driver {
        Driver::AsyncIo => {
            let executor = Rc::new(async_executor::LocalExecutor::new());
            let run = scenario.run(AsyncIo, Rc::clone(&executor));
            futures_lite::future::block_on(executor.run(run));
        }
        Driver::Tokio => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let local = Rc::new(tokio::task::LocalSet::new());
            let run = scenario.run(Tokio, Rc::clone(&local));
            local.block_on(&runtime, run);
        }
        Driver::Compio => {
            let runtime = Rc::new(executor_core::compio::Runtime::new().expect("compio runtime"));
            let run = scenario.run(Compio, Rc::clone(&runtime));
            runtime.block_on(run);
        }
    })
}

/// Stop a scenario by dropping the only sender, then wait for it to drain.
fn stop(sender: async_channel::Sender<()>, server: thread::JoinHandle<()>) {
    drop(sender);
    server.join().expect("the server thread drains and returns");
}

/// `Hyper` itself, on cleartext TCP.
struct Plain {
    listener: std::net::TcpListener,
    stopped: Receiver<()>,
    file: PathBuf,
}

impl Scenario for Plain {
    async fn run<I: Io, Ex: LocalExecutor + 'static>(self, io: I, executor: Rc<Ex>) {
        let listener = io.tcp_listener(self.listener).expect("register listener");
        let stopped = self.stopped;
        Hyper
            .serve(io, executor, listener, App::new(self.file), async move {
                stopped.recv().await.ok();
            })
            .await;
    }
}

/// TLS on TCP with ALPN choosing the protocol, and HTTP/3 on UDP beside it when `h3` is given.
struct Tls {
    listener: std::net::TcpListener,
    acceptor: TlsAcceptor,
    stopped: Receiver<()>,
    file: PathBuf,
    h3: Option<H3>,
}

struct H3 {
    socket: std::net::UdpSocket,
    config: http_kit_h3::ServerConfig,
}

impl Scenario for Tls {
    async fn run<I: Io, Ex: LocalExecutor + 'static>(self, io: I, executor: Rc<Ex>) {
        let app = App::new(self.file);
        let alt_svc = self
            .h3
            .as_ref()
            .map(|h3| AltSvc::h3(h3.socket.local_addr().expect("udp address").port()));

        let tcp = {
            let listener = io.tcp_listener(self.listener).expect("register listener");
            let (stop, stopping) = async_channel::bounded::<Infallible>(1);
            let (live, drained) = async_channel::bounded::<Infallible>(1);
            let executor = Rc::clone(&executor);
            let stopped = self.stopped.clone();
            let app = app.clone();
            async move {
                loop {
                    let accept = pin!(listener.accept());
                    let (stream, peer) = match select(accept, pin!(stopped.recv())).await {
                        Either::Left((accepted, _)) => accepted.expect("accept"),
                        Either::Right(_) => break,
                    };
                    let mut service = IntoService::new(app.clone()).with_peer_addr(peer);
                    if let Some(alt_svc) = &alt_svc {
                        service = service.with_alt_svc(alt_svc);
                    }
                    let acceptor = self.acceptor.clone();
                    let connection_executor = Rc::clone(&executor);
                    let stopping = stopping.clone();
                    let live = live.clone();
                    executor
                        .spawn_local(async move {
                            let _live = live;
                            let tls = acceptor
                                .accept(stream.into_poll())
                                .await
                                .expect("TLS handshake");
                            let protocol = match tls.get_ref().1.alpn_protocol() {
                                Some(b"h2") => Protocol::Http2,
                                _ => Protocol::Http1,
                            };
                            serve_connection(tls, protocol, service, connection_executor, async {
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
        };

        let udp = async {
            let Some(h3) = self.h3 else {
                return;
            };
            let socket = io.udp_socket(h3.socket).expect("register the UDP socket");
            let server =
                http_kit_h3::Server::new(socket, io.clone(), &h3.config).expect("h3 server");
            let stopped = self.stopped;
            // The same endpoint, answering over HTTP/3.
            let service = move |request: Request| {
                let mut app = app.clone();
                async move {
                    let mut request = request;
                    match app.respond(&mut request).await {
                        Ok(response) => response,
                        Err(error) => error_response(&error),
                    }
                }
            };
            server
                .serve_until(&service, async move {
                    stopped.recv().await.ok();
                })
                .await
                .expect("h3 server");
        };

        zip(tcp, udp).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------------------------

type Fetched = (StatusCode, hyper::HeaderMap, String);

/// One `GET` over `stream`, speaking HTTP/2 when `h2`, HTTP/1.1 otherwise.
async fn fetch<S>(stream: S, h2: bool, scheme: &str, path: &str) -> Fetched
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = FuturesIo::new(stream);
    let request = hyper::Request::get(format!("{scheme}://localhost{path}"))
        .body(Empty::<hyper::body::Bytes>::new())
        .expect("request");
    let response = if h2 {
        let (mut send, connection) = hyper::client::conn::http2::handshake(SmolExecutor, io)
            .await
            .expect("h2 handshake");
        smol::spawn(connection).detach();
        send.send_request(request).await.expect("h2 request")
    } else {
        let (mut send, connection) = hyper::client::conn::http1::handshake(io)
            .await
            .expect("h1 handshake");
        smol::spawn(connection).detach();
        send.send_request(request).await.expect("h1 request")
    };
    let (parts, body) = response.into_parts();
    let body = body.collect().await.expect("body").to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8(body.to_vec()).expect("text body"),
    )
}

async fn fetch_plain(addr: SocketAddr, h2: bool, path: &str) -> Fetched {
    let stream = TcpStream::connect(addr).await.expect("connect");
    fetch(stream, h2, "http", path).await
}

async fn fetch_tls(connector: &TlsConnector, addr: SocketAddr, alpn: &str, path: &str) -> Fetched {
    let stream = TcpStream::connect(addr).await.expect("connect");
    let tls = connector
        .clone()
        .connect(
            ServerName::try_from("localhost").expect("a DNS name"),
            stream,
        )
        .await
        .expect("TLS handshake");
    let negotiated = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
    assert_eq!(negotiated.as_deref(), Some(alpn.as_bytes()), "ALPN");
    fetch(tls, alpn == "h2", "https", path).await
}

// ---------------------------------------------------------------------------------------------
// TLS material
// ---------------------------------------------------------------------------------------------

struct Pki {
    cert_pem: String,
    key_pem: String,
    acceptor: TlsAcceptor,
    connector: Box<dyn Fn(&str) -> TlsConnector>,
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// A self-signed certificate for `localhost`, a rustls acceptor offering h2 and http/1.1, and
/// connectors trusting it that each offer one protocol.
fn pki() -> Pki {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("certificate");
    let der = certified.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::try_from(certified.signing_key.serialize_der())
        .expect("private key");

    let mut server = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .expect("server config");
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let connector = move |alpn: &str| {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(der.clone()).expect("the certificate is a root");
        let mut config = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![alpn.as_bytes().to_vec()];
        TlsConnector::from(Arc::new(config))
    };

    Pki {
        cert_pem: certified.cert.pem(),
        key_pem: certified.signing_key.serialize_pem(),
        acceptor: TlsAcceptor::from(Arc::new(server)),
        connector: Box::new(connector),
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

macro_rules! on_each_driver {
    ($scenario:ident) => {
        mod $scenario {
            #[test]
            fn on_async_io() {
                super::$scenario(super::Driver::AsyncIo);
            }

            #[test]
            fn on_tokio() {
                super::$scenario(super::Driver::Tokio);
            }

            #[test]
            fn on_compio() {
                super::$scenario(super::Driver::Compio);
            }
        }
    };
}

on_each_driver!(cleartext);
on_each_driver!(alpn);
on_each_driver!(h2_beside_h3);

/// h1 and h2c on one cleartext listener: the sniff picks the protocol, a file-region body is read
/// into bytes at the hyper boundary, state survives across connections, and an upgrade reaches its
/// session with the bytes sent behind the request intact.
fn cleartext(driver: Driver) {
    let (_dir, file) = served_file();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("address");
    let (stop_tx, stopped) = async_channel::bounded::<()>(1);
    let server = start(
        driver,
        Plain {
            listener,
            stopped,
            file,
        },
    );

    smol::block_on(async {
        let (status, headers, body) = fetch_plain(addr, false, "/hello").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("{:?} {CJK}", Version::HTTP_11));
        assert_eq!(headers[header::CONTENT_LENGTH], body.len().to_string());

        let (status, _, body) = fetch_plain(addr, true, "/hello").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("{:?} {CJK}", Version::HTTP_2));

        for h2 in [false, true] {
            let (status, _, body) = fetch_plain(addr, h2, "/file").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, CJK, "h2: {h2}");
        }

        // Four requests so far, and this one, went through the one endpoint's `Rc`.
        let (_, _, count) = fetch_plain(addr, false, "/count").await;
        assert_eq!(count, "5");

        let (_, _, upgradable) = fetch_plain(addr, false, "/upgradable").await;
        assert_eq!(upgradable, "true");
        let (_, _, upgradable) = fetch_plain(addr, true, "/upgradable").await;
        assert_eq!(upgradable, "false", "an h2 stream cannot be upgraded");

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(
                b"GET /echo HTTP/1.1\r\nHost: localhost\r\nConnection: upgrade\r\n\
                  Upgrade: echo\r\n\r\nping",
            )
            .await
            .expect("write the upgrade request and the bytes behind it");
        let mut raw = Vec::new();
        let mut buf = [0u8; 256];
        let expected = b"readyping";
        let body_start = loop {
            let n = stream.read(&mut buf).await.expect("read");
            assert!(n > 0, "the connection closed before the upgrade finished");
            raw.extend_from_slice(&buf[..n]);
            if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                if raw.len() >= end + 4 + expected.len() {
                    break end + 4;
                }
            }
        };
        let head = String::from_utf8_lossy(&raw[..body_start]);
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        assert_eq!(&raw[body_start..], expected);
    });

    stop(stop_tx, server);
}

/// h1 and h2 over TLS, chosen by ALPN alone, on a listener that never sniffs.
fn alpn(driver: Driver) {
    let (_dir, file) = served_file();
    let pki = pki();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("address");
    let (stop_tx, stopped) = async_channel::bounded::<()>(1);
    let server = start(
        driver,
        Tls {
            listener,
            acceptor: pki.acceptor.clone(),
            stopped,
            file,
            h3: None,
        },
    );

    smol::block_on(async {
        for (alpn, version) in [("h2", Version::HTTP_2), ("http/1.1", Version::HTTP_11)] {
            let connector = (pki.connector)(alpn);
            let (status, headers, body) = fetch_tls(&connector, addr, alpn, "/hello").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, format!("{version:?} {CJK}"));
            assert!(
                !headers.contains_key(header::ALT_SVC),
                "no h3 endpoint was announced"
            );

            let (_, _, body) = fetch_tls(&connector, addr, alpn, "/file").await;
            assert_eq!(body, CJK);
        }
    });

    stop(stop_tx, server);
}

/// One application serving h2 over TCP and h3 over UDP at the same time: the TCP responses carry
/// `Alt-Svc` naming the UDP port, and an h3 client following it reaches the same endpoint.
fn h2_beside_h3(driver: Driver) {
    let (_dir, file) = served_file();
    let pki = pki();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("address");
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind UDP");
    let udp_port = udp.local_addr().expect("UDP address").port();
    let (stop_tx, stopped) = async_channel::bounded::<()>(1);
    let server = start(
        driver,
        Tls {
            listener,
            acceptor: pki.acceptor.clone(),
            stopped,
            file,
            h3: Some(H3 {
                socket: udp,
                config: http_kit_h3::ServerConfig::new(
                    http_kit_h3::Identity::from_pem(
                        pki.cert_pem.clone().into_bytes(),
                        pki.key_pem.clone().into_bytes(),
                    ),
                    16 * 1024,
                ),
            }),
        },
    );

    smol::block_on(async {
        let (status, headers, body) = fetch_tls(&(pki.connector)("h2"), addr, "h2", "/hello").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("{:?} {CJK}", Version::HTTP_2));
        let alt_svc = headers[header::ALT_SVC].to_str().expect("visible ASCII");
        assert_eq!(alt_svc, format!("h3=\":{udp_port}\"; ma=86400"));

        // Follow it: the port comes from the header, not from what the test bound.
        let port: u16 = alt_svc
            .split_once("\":")
            .and_then(|(_, rest)| rest.split_once('"'))
            .and_then(|(port, _)| port.parse().ok())
            .expect("an h3 authority in Alt-Svc");

        let mut client = http_kit_h3::Client::new(&http_kit_h3::ClientConfig::new(
            http_kit_h3::Roots {
                system: false,
                pem: vec![pki.cert_pem.clone().into_bytes()],
            },
            16 * 1024,
        ))
        .expect("h3 client");
        let socket = AsyncIo
            .bind_udp(([127, 0, 0, 1], 0).into())
            .expect("client socket");
        let (send, connection) = client
            .connect(socket, AsyncIo, "localhost", ([127, 0, 0, 1], port).into())
            .expect("h3 connection");
        let exchange = async move {
            let mut request = Request::new(Body::empty());
            *request.uri_mut() = format!("https://localhost:{port}/hello")
                .parse()
                .expect("uri");
            let response = send.send(request).await.expect("h3 response");
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().into_bytes().await.expect("h3 body");
            drop(send);
            String::from_utf8(body.to_vec()).expect("text body")
        };
        let (body, ran) = zip(exchange, connection.run()).await;
        ran.expect("the h3 connection ends cleanly");
        assert_eq!(body, format!("{:?} {CJK}", Version::HTTP_3));

        // The endpoint's `Rc` is shared by both protocols: the h2 and the h3 request counted.
        let (_, _, count) = fetch_tls(&(pki.connector)("h2"), addr, "h2", "/count").await;
        assert_eq!(count, "3");
    });

    stop(stop_tx, server);
}
