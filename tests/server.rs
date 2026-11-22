//! A `!Send` router served through [`skyzen::Server`], by both of Skyzen's servers.
//!
//! The handler below counts requests in an `Rc<Cell<_>>`, so neither it nor the router holding it
//! can cross a thread. What crosses is the [`EndpointFactory`]: the serving thread asks it for the
//! router, then hands that router to the server along with listeners and a thread-local executor.
//! These threads run `async-io`, not compio, so the context they hand the factory says so.

use futures_lite::future::zip;
use http_kit::{Body, Request, StatusCode};
use http_kit_h2::{handshake_stream, ClientConfig};
use http_kit_io::{async_io::AsyncIo, Dial as _, FromStd as _, Listen};
use skyzen::{
    hyper::Hyper,
    routing::{CreateRouteNode, Route, Router},
    runtime::ServingThread,
    EndpointFactory, Server,
};
use skyzen_h1::H1;
use skyzen_h2::H2;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use std::{cell::Cell, net::SocketAddr, rc::Rc};

/// The routes every server here serves: one handler whose state is an `Rc`.
fn counting_router() -> Router {
    let served = Rc::new(Cell::new(0_u32));
    Route::new(("/count".at(move || {
        let served = Rc::clone(&served);
        async move {
            served.set(served.get() + 1);
            served.get().to_string()
        }
    }),))
    .build()
}

/// Serve the factory's router with `server` on a thread of its own, and return the bound address.
fn start<S>(server: S, factory: impl EndpointFactory<ServingThread>) -> SocketAddr
where
    S: Server<AsyncIo, Listeners = <AsyncIo as Listen>::Listener> + Send + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("local addr");

    std::thread::spawn(move || {
        let executor = Rc::new(async_executor::LocalExecutor::new());
        let listener = AsyncIo.tcp_listener(listener).expect("register listener");
        futures_lite::future::block_on(executor.run(async {
            let endpoint = factory.endpoint(ServingThread::without_compio()).await;
            server
                .serve(
                    AsyncIo,
                    Rc::clone(&executor),
                    listener,
                    endpoint,
                    std::future::pending(),
                )
                .await;
        }));
    });

    addr
}

/// One `GET` on a fresh connection; returns the whole response as text.
async fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = smol::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    String::from_utf8(response).expect("the response is text")
}

/// Two requests on two connections reach the same `Rc`, so the second one sees the first.
fn assert_counts(addr: SocketAddr) {
    smol::block_on(async {
        for expected in ["1", "2"] {
            let response = get(addr, "/count").await;
            assert!(
                response.starts_with("HTTP/1.1 200"),
                "expected a 200, got: {response}"
            );
            assert!(
                response.ends_with(&format!("\r\n\r\n{expected}")),
                "expected the body {expected}, got: {response}"
            );
        }
    });
}

#[test]
fn an_rc_holding_handler_serves_through_our_h1_server() {
    assert_counts(start(H1::default(), |_: ServingThread| async {
        counting_router()
    }));
}

#[test]
fn an_rc_holding_handler_serves_through_our_h2_server() {
    let addr = start(H2::default(), |_: ServingThread| async {
        counting_router()
    });
    smol::block_on(async move {
        for expected in ["1", "2"] {
            let stream = AsyncIo.connect(addr).await.expect("connect");
            let (send, conn) = handshake_stream(stream, AsyncIo, ClientConfig::default());
            let mut req = Request::new(Body::empty());
            *req.uri_mut() = format!("http://{addr}/count").parse().unwrap();
            let (ran, body) = zip(conn.run(), async move {
                let resp = send.send(req).await.expect("h2 request");
                assert_eq!(resp.status(), StatusCode::OK);
                let bytes = resp.into_body().into_bytes().await.expect("h2 body");
                drop(send);
                bytes
            })
            .await;
            ran.expect("h2 connection");
            assert_eq!(&body[..], expected.as_bytes());
        }
    });
}

#[test]
fn an_rc_holding_handler_serves_through_the_hyper_server() {
    assert_counts(start(Hyper, |_: ServingThread| async { counting_router() }));
}
