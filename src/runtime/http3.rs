//! HTTP/3 over QUIC for the native server.
//!
//! `quinn` supplies the transport on its `runtime-smol` backend (the native
//! runtime is smol, so no tokio runtime is linked), `h3` frames the requests,
//! `h3-quinn` adapts one to the other. One bound UDP socket is a
//! [`quinn::Endpoint`]; each accepted connection becomes an
//! `h3::server::Connection`; each accepted request gets its own driver task
//! feeding the request body through a bounded channel — the bound, plus QUIC
//! flow control, is the backpressure the TCP path gets for free.
//!
//! The driver task tree mirrors the TCP loop: the accept task, every connection
//! task and every request task holds a clone of the accept loop's drain token,
//! so `SHUTDOWN_GRACE_PERIOD` accounts for them exactly the same way.

use std::{collections::VecDeque, convert::Infallible, net::SocketAddr, sync::Arc};

use async_channel::{bounded, Receiver, Sender};
use executor_core::{AnyExecutor, Executor as CoreExecutor, Task as _};
use futures_util::{FutureExt, StreamExt};
use http_kit::BodyError;
use tracing::{debug, error, info};

use crate::{
    extract::PeerAddr,
    runtime::{context::ShutdownGuard, WorkerContext},
    Endpoint,
};

/// The decoded request-header budget enforced *during* QPACK decode.
///
/// h3's default is `VarInt::MAX` — no limit at all, which is the `FoxIO`
/// "Header Accordions" finding: a small HEADERS frame expands into a `HeaderMap` that
/// panics at 32,768 entries. `decode_stateless` checks the cumulative
/// name+value+32B-per-field size as it decodes, so a finite limit set here is
/// the complete mitigation — 64 KiB, matching the value upstream's fix PR
/// picks as the new default.
const MAX_FIELD_SECTION_SIZE: u64 = 64 * 1024;

/// Body chunks in flight between the QUIC stream and the handler at once.
/// Beyond this the driver stops reading and flow control stalls the client —
/// the same shape a TCP receive window gives the hyper path.
const BODY_CHANNEL_DEPTH: usize = 8;

/// The request stream h3 hands back over quinn: bidirectional, `bytes` chunks.
type RequestStream = h3::server::RequestStream<h3_quinn::BidiStream<bytes::Bytes>, bytes::Bytes>;

/// A UDP-bound QUIC endpoint serving h3, plus the `Alt-Svc` value the TCP
/// listener stamps on responses so clients learn the upgrade exists.
#[derive(Debug)]
pub struct Http3Server {
    endpoint: quinn::Endpoint,
    alt_svc: http::HeaderValue,
}

impl Http3Server {
    /// Bind a UDP socket on `addr` — the TCP listener's own address, so h3
    /// lands on the port HTTPS clients already dial.
    pub fn bind(addr: SocketAddr, server_config: quinn::ServerConfig) -> std::io::Result<Self> {
        let socket = std::net::UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(server_config),
            socket,
            Arc::new(quinn::SmolRuntime),
        )?;
        let alt_svc =
            http::HeaderValue::from_str(&format!("h3=\":{}\"", endpoint.local_addr()?.port()))
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        Ok(Self { endpoint, alt_svc })
    }

    /// The `Alt-Svc` header value advertising this listener, e.g. `h3=":8443"`.
    pub fn alt_svc(&self) -> http::HeaderValue {
        self.alt_svc.clone()
    }

    /// Accept QUIC connections until `stop` closes, then let the endpoint idle
    /// out so peers see a clean close rather than an idle-timeout death.
    pub async fn serve<Exec, E>(
        self,
        executor: Arc<Exec>,
        endpoint: E,
        guard: Sender<Infallible>,
        stop: Receiver<Infallible>,
    ) where
        Exec: CoreExecutor + 'static,
        E: Endpoint + Clone + Send + Sync + 'static,
    {
        loop {
            futures_util::select! {
                _ = stop.recv().fuse() => break,
                incoming = self.endpoint.accept().fuse() => {
                    let Some(incoming) = incoming else {
                        break;
                    };
                    let peer = incoming.remote_address();
                    let endpoint = endpoint.clone();
                    let conn_exec = executor.clone();
                    let conn_guard = guard.clone();
                    let conn_stop = stop.clone();
                    executor.spawn(async move {
                        // Held for the connection's life so the shutdown drain
                        // sees it, like a TCP connection task.
                        let _guard = conn_guard.clone();
                        let conn = match incoming.await {
                            Ok(conn) => conn,
                            Err(error) => {
                                debug!("QUIC handshake from {peer} failed: {error}");
                                return;
                            }
                        };
                        drive_connection(conn, peer, endpoint, conn_exec, conn_guard, conn_stop)
                            .await;
                    })
                    .detach();
                }
            }
        }
        self.endpoint.wait_idle().await;
    }
}

/// Drive one QUIC connection: build the h3 session on it, then accept requests
/// until the peer goes away or `stop` asks for a GOAWAY.
async fn drive_connection<Exec, E>(
    conn: quinn::Connection,
    peer: SocketAddr,
    endpoint: E,
    executor: Arc<Exec>,
    guard: Sender<Infallible>,
    stop: Receiver<Infallible>,
) where
    Exec: CoreExecutor + 'static,
    E: Endpoint + Clone + Send + Sync + 'static,
{
    let mut h3_conn = match h3::server::builder()
        .max_field_section_size(MAX_FIELD_SECTION_SIZE)
        .build(h3_quinn::Connection::new(conn))
        .await
    {
        Ok(conn) => conn,
        Err(error) => {
            debug!("h3 settings exchange with {peer} failed: {error}");
            return;
        }
    };

    let shared: Arc<AnyExecutor> = Arc::new(AnyExecutor::new(executor.clone()));
    let dispatch = |resolver| {
        executor
            .spawn(drive_request(
                resolver,
                peer,
                endpoint.clone(),
                shared.clone(),
                WorkerContext::new(shared.clone(), ShutdownGuard(guard.clone())),
            ))
            .detach();
    };

    loop {
        futures_util::select! {
            _ = stop.recv().fuse() => break,
            accepted = h3_conn.accept().fuse() => match accepted {
                Ok(Some(resolver)) => dispatch(resolver),
                Ok(None) => return,
                Err(error) => {
                    debug!("h3 connection from {peer} failed: {error}");
                    return;
                }
            },
        }
    }

    // GOAWAY past the last stream the client opened — nothing new — then keep
    // answering until the peer closes so requests in flight finish.
    if let Err(error) = h3_conn.shutdown(0).await {
        debug!("h3 GOAWAY to {peer} failed: {error}");
        return;
    }
    while let Ok(Some(resolver)) = h3_conn.accept().await {
        dispatch(resolver);
    }
}

/// Answer one accepted h3 request: feed its body to the handler, write the
/// response back on the same stream.
async fn drive_request<E>(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, bytes::Bytes>,
    peer: SocketAddr,
    mut endpoint: E,
    executor: Arc<AnyExecutor>,
    worker: WorkerContext,
) where
    E: Endpoint + Send + Sync + 'static,
{
    let (head, mut h3_stream): (http::Request<()>, RequestStream) =
        match resolver.resolve_request().await {
            Ok(request_parts) => request_parts,
            Err(error) => {
                debug!("h3 request stream from {peer} failed before headers: {error}");
                return;
            }
        };

    let (body_tx, body_rx) = bounded(BODY_CHANNEL_DEPTH);
    let mut request = head.map(|()| crate::Body::from_stream(body_rx));
    request.extensions_mut().insert(executor);
    request.extensions_mut().insert(worker);
    request.extensions_mut().insert(PeerAddr(peer));

    let method = request.method().clone();
    let path = request.uri().path().to_owned();

    let response = match pump_request(&mut h3_stream, &mut endpoint, &mut request, body_tx).await {
        Ok(response) => {
            info!(
                method = method.as_str(),
                path = path.as_str(),
                status = response.status().as_u16(),
                "request completed"
            );
            response
        }
        Err(error) => {
            skyzen_core::log_endpoint_error(&error, &method, path.as_str());
            skyzen_core::error_response(&error)
        }
    };

    if let Err(error) = write_response(&mut h3_stream, response).await {
        debug!("h3 response to {peer} {method} {path} failed: {error}");
    }
}

/// Drive `respond` and the request-body pump concurrently.
///
/// The handler consumes the body off `body_rx` while the QUIC stream supplies
/// it — awaiting `respond` first would deadlock a handler that reads its body,
/// and buffering the whole body first would defeat streaming. The select loop
/// keeps at most one undelivered chunk plus the channel window in memory.
async fn pump_request<E>(
    stream: &mut RequestStream,
    endpoint: &mut E,
    request: &mut crate::Request,
    body_tx: Sender<Result<crate::utils::Bytes, BodyError>>,
) -> Result<crate::Response, E::Error>
where
    E: Endpoint,
{
    let mut respond = std::pin::pin!(endpoint.respond(request).fuse());
    let mut body_tx = Some(body_tx);
    let mut pending: VecDeque<crate::utils::Bytes> = VecDeque::new();
    let mut recv_error: Option<BodyError> = None;
    let mut recv_open = true;
    let mut deliver = true;

    loop {
        futures_util::select! {
            res = respond => break res,
            chunk = next_chunk(stream, recv_open && pending.is_empty() && recv_error.is_none())
                .fuse() =>
            {
                match chunk {
                    Some(Ok(bytes)) => pending.push_back(bytes),
                    // A failed stream will keep failing, so mark the recv side
                    // dead alongside the error: once it forwards, the channel
                    // closes and the handler's body stream ends.
                    Some(Err(error)) => {
                        recv_error = Some(error);
                        recv_open = false;
                    }
                    None => recv_open = false,
                }
            }
            () = forward_pending(
                &mut body_tx,
                &mut pending,
                &mut recv_error,
                recv_open,
                &mut deliver,
            )
            .fuse() => {}
        }
    }
}

/// `None` ends the request body; `open` parks this branch while there is no
/// delivery slot, which is what keeps the pump bounded.
async fn next_chunk(
    stream: &mut RequestStream,
    open: bool,
) -> Option<Result<crate::utils::Bytes, BodyError>> {
    use bytes::Buf;

    if !open {
        return std::future::pending().await;
    }
    match stream.recv_data().await {
        Ok(Some(mut buf)) => Some(Ok(buf.copy_to_bytes(buf.remaining()))),
        Ok(None) => None,
        Err(error) => Some(Err(BodyError::Other(Box::new(error)))),
    }
}

/// Resolve once the next queued item reaches the handler — or park when there
/// is nothing queued, or when the handler's side of the channel is gone.
///
/// `body_tx` is an `Option` so the loop can close the channel when the recv
/// side is done: the handler's body stream only learns the body ended by
/// seeing every sender drop, and an endpoint that reads its body to EOF would
/// deadlock without it.
async fn forward_pending(
    body_tx: &mut Option<Sender<Result<crate::utils::Bytes, BodyError>>>,
    pending: &mut VecDeque<crate::utils::Bytes>,
    recv_error: &mut Option<BodyError>,
    recv_open: bool,
    deliver: &mut bool,
) {
    // The stream finished (or the handler abandoned the body) and everything
    // queued was delivered: close the channel so `Body` yields `None`.
    let drained = pending.is_empty() && recv_error.is_none();
    if drained && (!recv_open || !*deliver) {
        *body_tx = None;
        return;
    }
    let Some(tx) = &*body_tx else {
        return std::future::pending::<()>().await;
    };
    if let Some(bytes) = pending.front() {
        // `send` consumes its argument, so the clone goes on the wire: if this
        // future is cancelled mid-send the original stays queued for the next
        // select iteration instead of silently losing a body chunk.
        *deliver = tx.send(Ok(bytes.clone())).await.is_ok();
        if *deliver {
            pending.pop_front();
        }
    } else if let Some(error) = recv_error.take() {
        *deliver = tx.send(Err(error)).await.is_ok();
    } else {
        std::future::pending::<()>().await;
    }
}

/// Write the handler's response: headers, then the body as DATA frames, then
/// the stream's FIN. A body-stream failure resets the stream instead of
/// pretending the truncated response is complete.
async fn write_response(
    stream: &mut RequestStream,
    response: crate::Response,
) -> Result<(), h3::error::StreamError> {
    let (parts, mut body) = response.into_parts();
    stream
        .send_response(http::Response::from_parts(parts, ()))
        .await?;
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(bytes) => stream.send_data(bytes).await?,
            Err(error) => {
                error!("response body stream failed: {error}");
                stream.stop_stream(h3::error::Code::H3_INTERNAL_ERROR);
                return Ok(());
            }
        }
    }
    stream.finish().await
}

#[cfg(test)]
mod tests {
    use std::{future::Future, net::IpAddr, net::Ipv4Addr};

    use executor_core::smol::SmolGlobal;
    use http_kit::error::BoxHttpError;

    use super::*;

    /// A self-signed leaf the client pins as its root, and both QUIC crypto
    /// configs — TLS 1.3 + ALPN `h3`, the same shape `build_http3` produces
    /// from PEM files.
    fn quic_configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
        use futures_rustls::rustls;

        let certified =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("rcgen");
        let cert_der = certified.cert.der().clone();
        let key_der =
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());

        let mut server_tls = rustls::ServerConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der)
        .expect("server tls");
        server_tls.alpn_protocols = vec![b"h3".to_vec()];

        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(cert_der)
            .expect("the generated cert is a valid root");
        let mut client_tls = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("client protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"h3".to_vec()];

        (
            quinn::ServerConfig::with_crypto(Arc::new(
                quinn::crypto::rustls::QuicServerConfig::try_from(server_tls).expect("quic server"),
            )),
            quinn::ClientConfig::new(Arc::new(
                quinn::crypto::rustls::QuicClientConfig::try_from(client_tls).expect("quic client"),
            )),
        )
    }

    /// Echoes the request body and reports the caller's `PeerAddr`, so one
    /// roundtrip exercises the body pump in both directions and the extension
    /// the h3 path stamps.
    #[derive(Clone)]
    struct EchoEndpoint;

    impl Endpoint for EchoEndpoint {
        type Error = BoxHttpError;

        fn respond(
            &mut self,
            request: &mut crate::Request,
        ) -> impl Future<Output = Result<crate::Response, Self::Error>> + Send {
            let peer = request
                .extensions()
                .get::<PeerAddr>()
                .map_or_else(|| "none".to_owned(), |PeerAddr(addr)| addr.to_string());
            let body = request.body_mut();
            async move {
                let mut collected = Vec::new();
                while let Some(chunk) = body.next().await {
                    let chunk = chunk.expect("the body pump must deliver");
                    collected.extend_from_slice(&chunk);
                }
                let mut response = crate::Response::new(crate::Body::from_bytes(collected));
                response.headers_mut().insert(
                    http::header::HeaderName::from_static("x-peer"),
                    http::HeaderValue::from_str(&peer).expect("peer header"),
                );
                Ok(response)
            }
        }
    }

    /// A real loopback roundtrip: rcgen cert, quinn server + client on the smol
    /// runtime, h3 request with a body, echoed body and peer address back.
    #[test]
    fn h3_request_roundtrips_body_and_peer_addr() {
        smol::future::block_on(async {
            let (server_config, client_config) = quic_configs();
            let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
            let server = Http3Server::bind(loopback, server_config).expect("bind");
            let addr = server.endpoint.local_addr().expect("local addr");

            let (guard_tx, _guard_rx) = bounded::<Infallible>(1);
            let (stop_tx, stop_rx) = bounded::<Infallible>(1);
            smol::spawn(server.serve(Arc::new(SmolGlobal), EchoEndpoint, guard_tx, stop_rx))
                .detach();

            let mut client = quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                None,
                std::net::UdpSocket::bind(loopback).expect("client socket"),
                Arc::new(quinn::SmolRuntime),
            )
            .expect("client endpoint");
            client.set_default_client_config(client_config);

            let conn = client
                .connect(addr, "localhost")
                .expect("connect")
                .await
                .expect("connected");
            let close_conn = conn.clone();
            let (driver, mut send_request) = h3::client::new(h3_quinn::Connection::new(conn))
                .await
                .expect("h3 client");
            let mut driver = driver;
            smol::spawn(futures_util::future::poll_fn(move |cx| {
                driver.poll_close(cx)
            }))
            .detach();

            let request = http::Request::builder()
                .method("POST")
                .uri("https://localhost/echo")
                .body(())
                .expect("request");
            let mut stream = send_request
                .send_request(request)
                .await
                .expect("send_request");
            stream
                .send_data(bytes::Bytes::from_static(b"ping-body"))
                .await
                .expect("send data");
            stream.finish().await.expect("finish send");

            let response = stream.recv_response().await.expect("recv response");
            assert_eq!(response.status(), http::StatusCode::OK);
            let peer = response
                .headers()
                .get(http::header::HeaderName::from_static("x-peer"))
                .expect("x-peer header")
                .to_str()
                .expect("peer header text")
                .to_owned();
            assert_ne!(peer, "none", "PeerAddr extension must reach the handler");

            let mut body = Vec::new();
            while let Some(chunk) = stream.recv_data().await.expect("recv data") {
                use bytes::Buf;
                let mut chunk = chunk;
                while chunk.has_remaining() {
                    let bytes = chunk.copy_to_bytes(chunk.remaining());
                    body.extend_from_slice(&bytes);
                }
            }
            assert_eq!(body, b"ping-body");

            drop(stop_tx);
            close_conn.close(quinn::VarInt::from_u32(0), b"done");
            client.wait_idle().await;
        });
    }
}
