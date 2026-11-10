//! End-to-end tests: drive [`skyzen_h1::H1`] over a localhost listener with a raw HTTP/1.1
//! client, on a server thread that owns a `!Send` endpoint.

use core::future::Future;
use std::{cell::Cell, net::SocketAddr, rc::Rc};

use futures_lite::{AsyncReadExt, AsyncWriteExt};
use http_kit::{error::BoxHttpError, header, http_error, Body, Request, Response, StatusCode};
use http_kit_io::{async_io::AsyncIo, Io};
use skyzen_core::{
    upgrade::{OnUpgrade, UpgradeAvailable},
    Endpoint, PeerAddr, Server,
};
use skyzen_h1::H1;
use smol::net::TcpStream;

http_error!(
    /// Server error whose internal detail must NOT reach the client.
    SecretDbError,
    StatusCode::INTERNAL_SERVER_ERROR,
    "db password is hunter2"
);

/// Counts the requests its thread served, in a `Cell` behind an `Rc` — state no `Send` router
/// could hold.
#[derive(Clone)]
struct CountingEndpoint {
    served: Rc<Cell<u32>>,
}

impl Endpoint for CountingEndpoint {
    type Error = BoxHttpError;

    fn respond(
        &mut self,
        request: &mut Request,
    ) -> impl Future<Output = Result<Response, Self::Error>> {
        let served = Rc::clone(&self.served);
        let upgradable = request.extensions().get::<UpgradeAvailable>().is_some();
        let peer = request.extensions().get::<PeerAddr>().copied();
        let path = request.uri().path().to_owned();
        async move {
            // Held across an await on purpose: the future is `!Send`.
            futures_lite::future::yield_now().await;
            served.set(served.get() + 1);
            match path.as_str() {
                "/count" => Ok(Response::new(Body::from(served.get().to_string()))),
                "/peer" => Ok(Response::new(Body::from(format!(
                    "{upgradable} {}",
                    peer.is_some_and(|peer| peer.ip().is_loopback())
                )))),
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
                    Ok(response)
                }
                _ => Err(Box::new(SecretDbError::new()) as Self::Error),
            }
        }
    }
}

/// Serve `H1` on a thread of its own, the endpoint built on that thread.
fn start_server() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("local addr");

    std::thread::spawn(move || {
        let executor = Rc::new(async_executor::LocalExecutor::new());
        let listener = AsyncIo.tcp_listener(listener).expect("register listener");
        let endpoint = CountingEndpoint {
            served: Rc::new(Cell::new(0)),
        };
        let server = H1::default();
        let serve = server.serve(
            AsyncIo,
            Rc::clone(&executor),
            listener,
            endpoint,
            core::future::pending(),
        );
        futures_lite::future::block_on(executor.run(serve));
    });

    addr
}

/// Serve `H1` on a compio runtime of its own — the completion-based driver, on the thread that
/// owns both the runtime and the endpoint.
fn start_compio_server() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("local addr");

    std::thread::spawn(move || {
        let runtime = Rc::new(executor_core::compio::Runtime::new().expect("compio runtime"));
        let serve_runtime = Rc::clone(&runtime);
        runtime.block_on(async move {
            let io = http_kit_io::compio::Compio;
            let listener = io.tcp_listener(listener).expect("register listener");
            let endpoint = CountingEndpoint {
                served: Rc::new(Cell::new(0)),
            };
            H1::default()
                .serve(
                    io,
                    serve_runtime,
                    listener,
                    endpoint,
                    core::future::pending(),
                )
                .await;
        });
    });

    addr
}

/// Read one response head off `stream`, returning it and whatever body bytes followed.
async fn read_head(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf).await.expect("read response");
        assert!(n > 0, "connection closed before the response head ended");
        raw.extend_from_slice(&buf[..n]);
        if let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8(raw[..end].to_vec()).expect("head is UTF-8");
            return (head, raw[end + 4..].to_vec());
        }
    }
}

async fn get(addr: SocketAddr, path: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a response head");
    (
        String::from_utf8(raw[..end].to_vec()).expect("head is UTF-8"),
        raw[end + 4..].to_vec(),
    )
}

#[test]
fn a_thread_local_endpoint_keeps_its_state_across_connections() {
    let addr = start_server();
    smol::block_on(async {
        let (head, first) = get(addr, "/count").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let (_, second) = get(addr, "/count").await;
        assert_eq!(first, b"1");
        assert_eq!(second, b"2");
    });
}

#[test]
fn the_completion_driver_serves_the_same_endpoint() {
    let addr = start_compio_server();
    smol::block_on(async {
        let (head, first) = get(addr, "/count").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let (_, second) = get(addr, "/count").await;
        assert_eq!(first, b"1");
        assert_eq!(second, b"2");
    });
}

#[test]
fn requests_carry_the_peer_and_the_upgrade_marker() {
    let addr = start_server();
    smol::block_on(async {
        let (_, body) = get(addr, "/peer").await;
        assert_eq!(body, b"true true");
    });
}

#[test]
fn a_server_error_is_redacted() {
    let addr = start_server();
    smol::block_on(async {
        let (head, body) = get(addr, "/boom").await;
        assert!(head.starts_with("HTTP/1.1 500"), "{head}");
        assert_eq!(body, br#"{"error":"Internal server error"}"#);
    });
}

#[test]
fn an_upgrade_hands_the_connection_to_the_session_with_early_bytes_intact() {
    let addr = start_server();
    smol::block_on(async {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        // The first bytes of the upgraded protocol follow the request in the same write, so
        // they arrive in the server's receive buffer, not on the socket.
        stream
            .write_all(
                b"GET /echo HTTP/1.1\r\nHost: localhost\r\nConnection: upgrade\r\n\
                  Upgrade: echo\r\n\r\nping",
            )
            .await
            .expect("write");

        let (head, mut after) = read_head(&mut stream).await;
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        while after.len() < b"readyping".len() {
            let mut buf = [0u8; 16];
            let n = stream.read(&mut buf).await.expect("read upgraded");
            assert!(n > 0, "upgraded connection closed early");
            after.extend_from_slice(&buf[..n]);
        }
        assert_eq!(after, b"readyping");
    });
}
