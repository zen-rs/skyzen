//! End-to-end tests: drive [`skyzen_h2::H2`] over a localhost listener with `http-kit-h2`,
//! on both `async-io` (readiness) and `compio` (`io_uring` / IOCP completion), serving a `!Send`
//! endpoint.

use core::future::Future;
use std::{cell::Cell, convert::Infallible, net::SocketAddr, rc::Rc};

use futures_lite::{future::zip, AsyncReadExt, AsyncWriteExt};
use http_kit::{
    error::BoxHttpError, header, http_error, Body, Method, Request, Response, StatusCode,
};
use http_kit_h2::{handshake_stream, ClientConfig, Protocol};
use http_kit_io::{async_io::AsyncIo, compio::Compio, Dial as _, FromStd as _};
use skyzen_core::{
    upgrade::{ConnectProtocol, OnUpgrade, UpgradeAvailable},
    AltSvc, Endpoint, PeerAddr, PendingResponseHeaders, Server,
};
use skyzen_h2::H2;

const CJK: &str = "你好，世界。こんにちは、世界。안녕하세요, 세계.";

http_error!(
    /// Server error whose internal detail must NOT reach the client.
    SecretDbError,
    StatusCode::INTERNAL_SERVER_ERROR,
    "db password is hunter2"
);

/// Counts the requests its thread served in an `Rc<Cell<u32>>` — state no `Send` router can hold.
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
        let connect_proto = request
            .extensions()
            .get::<ConnectProtocol>()
            .map(|p| p.as_str().to_owned());
        let peer = request.extensions().get::<PeerAddr>().copied();
        let path = request.uri().path().to_owned();
        let method = request.method().clone();
        let incoming = if path == "/echo" && method == Method::POST {
            Some(core::mem::replace(request.body_mut(), Body::empty()))
        } else {
            None
        };
        if path == "/boom" {
            let mut pending = PendingResponseHeaders::default();
            pending.0.insert(
                header::HeaderName::from_static("x-request-id"),
                header::HeaderValue::from_static("req-42"),
            );
            request.extensions_mut().insert(pending);
        }

        async move {
            futures_lite::future::yield_now().await;
            served.set(served.get() + 1);
            match path.as_str() {
                "/count" => Ok(Response::new(Body::from(served.get().to_string()))),
                "/peer" => Ok(Response::new(Body::from(format!(
                    "{upgradable} {}",
                    peer.is_some_and(|p| p.ip().is_loopback())
                )))),
                "/echo" => {
                    let bytes = incoming
                        .expect("POST /echo body")
                        .into_bytes()
                        .await
                        .expect("read request body");
                    Ok(Response::new(Body::from_bytes(bytes)))
                }
                "/tunnel" => {
                    assert_eq!(method, Method::CONNECT);
                    assert_eq!(connect_proto.as_deref(), Some("websocket"));
                    let mut response = Response::new(Body::empty());
                    *response.status_mut() = StatusCode::OK;
                    response
                        .extensions_mut()
                        .insert(OnUpgrade::new(|mut io| async move {
                            io.write_all(CJK.as_bytes()).await.expect("greet");
                            io.flush().await.expect("flush greet");
                            let mut buf = vec![0_u8; CJK.len()];
                            io.read_exact(&mut buf).await.expect("read tunnel");
                            io.write_all(&buf).await.expect("echo tunnel");
                            io.close().await.expect("close tunnel");
                        }));
                    Ok(response)
                }
                _ => Err(Box::new(SecretDbError::new()) as Self::Error),
            }
        }
    }
}

fn start_async_io_server(server: H2) -> (SocketAddr, async_channel::Sender<Infallible>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("local addr");
    let (stop_tx, stop_rx) = async_channel::bounded::<Infallible>(1);

    std::thread::spawn(move || {
        let executor = Rc::new(async_executor::LocalExecutor::new());
        let listener = AsyncIo.tcp_listener(listener).expect("register listener");
        let endpoint = CountingEndpoint {
            served: Rc::new(Cell::new(0)),
        };
        let serve = server.serve(
            AsyncIo,
            Rc::clone(&executor),
            listener,
            endpoint,
            async move {
                let _ = stop_rx.recv().await;
            },
        );
        futures_lite::future::block_on(executor.run(serve));
    });

    (addr, stop_tx)
}

fn start_compio_server(server: H2) -> (SocketAddr, async_channel::Sender<Infallible>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("local addr");
    let (stop_tx, stop_rx) = async_channel::bounded::<Infallible>(1);

    std::thread::spawn(move || {
        let runtime = Rc::new(executor_core::compio::Runtime::new().expect("compio runtime"));
        let serve_runtime = Rc::clone(&runtime);
        runtime.block_on(async move {
            let io = Compio::default();
            let listener = io.tcp_listener(listener).expect("register listener");
            let endpoint = CountingEndpoint {
                served: Rc::new(Cell::new(0)),
            };
            server
                .serve(io, serve_runtime, listener, endpoint, async move {
                    let _ = stop_rx.recv().await;
                })
                .await;
        });
    });

    (addr, stop_tx)
}

#[test]
fn compio_and_async_io_multiplex_cjk_errors_alt_svc_and_extended_connect() {
    let configured = || H2::default().with_alt_svc(AltSvc::h3(8443).header_value());
    for (addr, _stop) in [
        start_async_io_server(configured()),
        start_compio_server(configured()),
    ] {
        let runtime = executor_core::compio::Runtime::new().expect("client compio runtime");
        runtime.block_on(async move {
            let io = Compio::default();
            let stream = io.connect(addr).await.expect("connect");
            let (send, conn) = handshake_stream(stream, io, ClientConfig::default());

            let client = async move {
                // 1. Concurrent multiplexed CJK POST streams on a single connection.
                let s1 = send.clone();
                let s2 = send.clone();
                let p1 = format!("一:{CJK}");
                let p2 = format!("二:{CJK}");
                let r1 = async {
                    let mut req = Request::new(Body::from(p1.clone()));
                    *req.method_mut() = Method::POST;
                    *req.uri_mut() = format!("http://{addr}/echo").parse().unwrap();
                    let resp = s1.send(req).await.expect("send 1");
                    assert_eq!(resp.status(), StatusCode::OK);
                    assert_eq!(resp.headers()[header::ALT_SVC], "h3=\":8443\"; ma=86400");
                    resp.into_body().into_bytes().await.expect("body 1")
                };
                let r2 = async {
                    let mut req = Request::new(Body::from(p2.clone()));
                    *req.method_mut() = Method::POST;
                    *req.uri_mut() = format!("http://{addr}/echo").parse().unwrap();
                    let resp = s2.send(req).await.expect("send 2");
                    assert_eq!(resp.status(), StatusCode::OK);
                    resp.into_body().into_bytes().await.expect("body 2")
                };
                let (b1, b2) = zip(r1, r2).await;
                assert_eq!(&b1[..], p1.as_bytes());
                assert_eq!(&b2[..], p2.as_bytes());

                // 2. PeerAddr is attached and HTTP/1.1 UpgradeAvailable is NOT attached.
                let mut peer_req = Request::new(Body::empty());
                *peer_req.uri_mut() = format!("http://{addr}/peer").parse().unwrap();
                let peer_resp = send.send(peer_req).await.expect("peer");
                let peer_body = peer_resp.into_body().into_bytes().await.expect("peer body");
                assert_eq!(&peer_body[..], b"false true");

                // 3. 5xx redaction + PendingResponseHeaders + Alt-Svc on error responses.
                let mut err_req = Request::new(Body::empty());
                *err_req.uri_mut() = format!("http://{addr}/boom").parse().unwrap();
                let err_resp = send.send(err_req).await.expect("boom");
                assert_eq!(err_resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(err_resp.headers()["x-request-id"], "req-42");
                assert_eq!(
                    err_resp.headers()[header::ALT_SVC],
                    "h3=\":8443\"; ma=86400"
                );
                let err_body = err_resp.into_body().into_bytes().await.expect("err body");
                assert_eq!(&err_body[..], br#"{"error":"Internal server error"}"#);

                // 4. `!Send` counter persisted across all four requests above.
                let mut cnt_req = Request::new(Body::empty());
                *cnt_req.uri_mut() = format!("http://{addr}/count").parse().unwrap();
                let cnt_resp = send.send(cnt_req).await.expect("count");
                let cnt_body = cnt_resp.into_body().into_bytes().await.expect("count body");
                assert_eq!(&cnt_body[..], b"5");

                // 5. Extended CONNECT (`:protocol = websocket`) full-duplex body tunnel.
                let (tun_tx, tun_rx) =
                    async_channel::bounded::<Result<http_kit::BodyFrame, http_kit::BodyError>>(4);
                let mut tun_req = Request::new(Body::from_frames(tun_rx));
                *tun_req.method_mut() = Method::CONNECT;
                *tun_req.uri_mut() = format!("http://{addr}/tunnel").parse().unwrap();
                tun_req
                    .extensions_mut()
                    .insert(Protocol::from_static("websocket"));
                let tun_resp = send.send(tun_req).await.expect("tunnel");
                assert_eq!(tun_resp.status(), StatusCode::OK);
                tun_tx
                    .send(Ok(http_kit::BodyFrame::Data(
                        http_kit::utils::Bytes::copy_from_slice(CJK.as_bytes()),
                    )))
                    .await
                    .expect("send tunnel payload");
                drop(tun_tx);
                let tun_out = tun_resp
                    .into_body()
                    .into_bytes()
                    .await
                    .expect("tunnel output");
                assert_eq!(&tun_out[..], format!("{CJK}{CJK}").as_bytes());

                drop(send);
            };

            let (ran, ()) = zip(conn.run(), client).await;
            ran.expect("h2 client connection");
        });
    }
}

#[test]
fn max_connections_closes_excess_and_recovers_after_release() {
    let (addr, _stop) =
        start_compio_server(H2::default().with_max_connections(1.try_into().unwrap()));
    let runtime = executor_core::compio::Runtime::new().expect("compio runtime");
    runtime.block_on(async move {
        let io = Compio::default();
        let holder = io.connect(addr).await.expect("holder connect");

        let mut refused = false;
        for _ in 0..32 {
            let stream = io.connect(addr).await.expect("probe connect");
            let (send, conn) = handshake_stream(stream, io.clone(), ClientConfig::default());
            let mut req = Request::new(Body::empty());
            *req.uri_mut() = format!("http://{addr}/count").parse().unwrap();
            let (ran, ok) = zip(conn.run(), async move {
                let res = send.send(req).await;
                drop(send);
                match res {
                    Ok(resp) => resp.into_body().into_bytes().await.is_ok(),
                    Err(_) => false,
                }
            })
            .await;
            if ran.is_err() || !ok {
                refused = true;
                break;
            }
        }
        assert!(refused, "a full H2 server must close excess connections");

        drop(holder);
        let mut served = false;
        for _ in 0..32 {
            let stream = io.connect(addr).await.expect("retry connect");
            let (send, conn) = handshake_stream(stream, io.clone(), ClientConfig::default());
            let mut req = Request::new(Body::empty());
            *req.uri_mut() = format!("http://{addr}/count").parse().unwrap();
            let (ran, ok) = zip(conn.run(), async move {
                let res = send.send(req).await;
                drop(send);
                match res {
                    Ok(resp) => {
                        resp.status() == StatusCode::OK
                            && resp.into_body().into_bytes().await.is_ok()
                    }
                    Err(_) => false,
                }
            })
            .await;
            if ran.is_ok() && ok {
                served = true;
                break;
            }
        }
        assert!(served, "freed slot must accept new H2 connections");
    });
}
