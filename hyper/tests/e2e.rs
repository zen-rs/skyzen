//! End-to-end tests: drive [`skyzen_hyper::Hyper`] over a localhost listener with a raw
//! HTTP/1.1 client and assert on the bytes that actually go over the wire.

use core::future::Future;
use executor_core::smol::SmolGlobal;
use http_kit::error::BoxHttpError;
use http_kit::{http_error, Body, Request, Response, StatusCode};
use skyzen_core::{Endpoint, Server};
use skyzen_hyper::Hyper;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::net::{SocketAddr, TcpListener, TcpStream};

http_error!(
    /// Client error whose message must reach the client.
    TeapotError,
    StatusCode::IM_A_TEAPOT,
    "the teapot is busy"
);

http_error!(
    /// Server error whose internal detail must NOT reach the client.
    SecretDbError,
    StatusCode::INTERNAL_SERVER_ERROR,
    "db password is hunter2"
);

/// Trivial endpoint covering a fixed-size success body, a 4xx error, and a 5xx error.
#[derive(Clone)]
struct TestEndpoint;

impl Endpoint for TestEndpoint {
    type Error = BoxHttpError;

    fn respond(
        &mut self,
        request: &mut Request,
    ) -> impl Future<Output = Result<Response, Self::Error>> + Send {
        core::future::ready(match request.uri().path() {
            "/hello" => Ok(Response::new(Body::from_bytes("hello world"))),
            "/teapot" => Err(Box::new(TeapotError::new()) as Self::Error),
            _ => Err(Box::new(SecretDbError::new()) as Self::Error),
        })
    }
}

/// Bind a localhost listener, spawn `Hyper::serve` on it, and return the bound address.
async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("local addr");

    let connections = Box::pin(futures_util::stream::unfold(
        listener,
        |listener| async move {
            let result = listener.accept().await.map(|(stream, _)| stream);
            Some((result, listener))
        },
    ));

    smol::spawn(Hyper.serve(
        SmolGlobal,
        |error: std::io::Error| panic!("accept failed: {error}"),
        connections,
        TestEndpoint,
    ))
    .detach();

    addr
}

/// Issue a raw HTTP/1.1 GET and return (status line, lowercased header lines, body bytes).
async fn raw_get(addr: SocketAddr, path: &str) -> (String, Vec<String>, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response has a header/body separator");
    let head = String::from_utf8(raw[..split].to_vec()).expect("headers are UTF-8");
    let body = raw[split + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line").to_owned();
    let headers = lines.map(str::to_lowercase).collect();
    (status_line, headers, body)
}

fn header_value<'a>(headers: &'a [String], name: &str) -> Option<&'a str> {
    let prefix = format!("{name}:");
    headers
        .iter()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::trim)
}

#[test]
fn fixed_size_response_has_content_length() {
    smol::block_on(async {
        let addr = start_server().await;
        let (status_line, headers, body) = raw_get(addr, "/hello").await;

        assert!(status_line.starts_with("HTTP/1.1 200"), "{status_line}");
        assert_eq!(body, b"hello world");
        assert_eq!(header_value(&headers, "content-length"), Some("11"));
        assert_eq!(header_value(&headers, "transfer-encoding"), None);
    });
}

#[test]
fn server_error_body_is_redacted_json() {
    smol::block_on(async {
        let addr = start_server().await;
        let (status_line, headers, body) = raw_get(addr, "/boom").await;

        assert!(status_line.starts_with("HTTP/1.1 500"), "{status_line}");
        assert_eq!(
            header_value(&headers, "content-type"),
            Some("application/json")
        );
        assert_eq!(body, br#"{"error":"Internal server error"}"#);
        assert!(
            !body.windows(7).any(|window| window == b"hunter2"),
            "internal detail leaked to the client"
        );
    });
}

#[test]
fn client_error_keeps_its_message() {
    smol::block_on(async {
        let addr = start_server().await;
        let (status_line, headers, body) = raw_get(addr, "/teapot").await;

        assert!(status_line.starts_with("HTTP/1.1 418"), "{status_line}");
        assert_eq!(
            header_value(&headers, "content-type"),
            Some("application/json")
        );
        assert_eq!(body, br#"{"error":"the teapot is busy"}"#);
    });
}
