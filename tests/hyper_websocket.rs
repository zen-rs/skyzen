//! WebSocket integration tests for the Skyzen Hyper backend.

use async_tungstenite::{
    client_async,
    tokio::TokioAdapter,
    tungstenite::{
        client::IntoClientRequest, handshake::client::Response as ClientResponse, Message,
    },
    WebSocketStream,
};
use executor_core::AnyExecutor;
use futures_util::StreamExt;
use hyper::header::SEC_WEBSOCKET_PROTOCOL;
use hyper::server::conn::http1;
use skyzen::{
    routing::{CreateRouteNode, Route},
    websocket::WebSocketUpgrade,
};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::duplex;

type Error = Box<dyn std::any::Any + Send>;

/// Test executor that uses `tokio::spawn` to dispatch tasks within the current runtime
struct TestTokioExecutor;

impl executor_core::Executor for TestTokioExecutor {
    type Task<T: Send + 'static> = TestTokioTask<T>;

    fn spawn<Fut>(&self, fut: Fut) -> Self::Task<Fut::Output>
    where
        Fut: std::future::Future<Output: Send> + Send + 'static,
    {
        TestTokioTask(tokio::spawn(fut))
    }
}

struct TestTokioTask<T>(tokio::task::JoinHandle<T>);

impl<T: Send + 'static> std::future::Future for TestTokioTask<T> {
    type Output = T;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        match std::pin::Pin::new(&mut self.0).poll(cx) {
            Poll::Ready(Ok(v)) => Poll::Ready(v),
            Poll::Ready(Err(e)) => std::panic::resume_unwind(e.into_panic()),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: Send + 'static> executor_core::Task<T> for TestTokioTask<T> {
    fn poll_result(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<T, Error>> {
        use std::future::Future;
        use std::task::Poll;
        match std::pin::Pin::new(&mut self.0).poll(cx) {
            Poll::Ready(Ok(v)) => Poll::Ready(Ok(v)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.into_panic())),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn create_executor() -> Arc<AnyExecutor> {
    // For tests running on tokio, we use the current tokio runtime via tokio::spawn
    Arc::new(AnyExecutor::new(TestTokioExecutor))
}

/// Wrapper to adapt tokio's `DuplexStream` to hyper's Read/Write traits
struct TokioIo(tokio::io::DuplexStream);

impl hyper::rt::Read for TokioIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        use tokio::io::AsyncRead;
        let inner = &mut self.get_mut().0;
        let mut read_buf = tokio::io::ReadBuf::uninit(unsafe { buf.as_mut() });
        match Pin::new(inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled().len();
                unsafe { buf.advance(filled) };
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl hyper::rt::Write for TokioIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        use tokio::io::AsyncWrite;
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        use tokio::io::AsyncWrite;
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        use tokio::io::AsyncWrite;
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

async fn spawn_router<Req>(
    router: Route,
    request: Req,
) -> (
    WebSocketStream<TokioAdapter<tokio::io::DuplexStream>>,
    ClientResponse,
    tokio::task::JoinHandle<()>,
)
where
    Req: IntoClientRequest + Unpin,
{
    let router = router.build();
    let executor = create_executor();
    let (client_stream, server_stream) = duplex(1024);
    let handle = tokio::spawn(async move {
        let io = TokioIo(server_stream);
        let service = skyzen_hyper::IntoService::new(router, executor);
        let builder = http1::Builder::new();

        if let Err(error) = builder.serve_connection(io, service).with_upgrades().await {
            panic!("websocket server failure: {error}");
        }
    });

    let (client, response) = client_async(request, TokioAdapter::new(client_stream))
        .await
        .expect("connect to websocket server");

    (client, response, handle)
}

#[tokio::test]
async fn websocket_roundtrip_over_hyper() {
    let (mut client, _, handle) = spawn_router(
        Route::new(("/ws".ws(|mut socket| async move {
            while let Some(Ok(message)) = socket.next().await {
                if let Some(text) = message.into_text() {
                    let _ = socket.send_text(text).await;
                }
            }
        }),)),
        "ws://localhost/ws",
    )
    .await;

    client
        .send(Message::text("hello"))
        .await
        .expect("send message");
    let reply = client
        .next()
        .await
        .expect("missing reply")
        .expect("websocket frame");
    assert_eq!(reply.into_text().unwrap(), "hello");

    let _ = client.close(None).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn websocket_negotiates_protocol_with_standard_client() {
    let mut request = "ws://localhost/protocols"
        .into_client_request()
        .expect("build websocket request");
    request.headers_mut().append(
        SEC_WEBSOCKET_PROTOCOL,
        "chat, superchat"
            .parse()
            .expect("parse Sec-WebSocket-Protocol header"),
    );

    let (mut client, response, handle) = spawn_router(
        Route::new(("/protocols".at(|upgrade: WebSocketUpgrade| async move {
            upgrade
                .protocols(["chat", "superchat"])
                .on_upgrade(|mut socket| async move {
                    let _ = socket.send_text("protocol-ok").await;
                })
        }),)),
        request,
    )
    .await;

    let negotiated_protocol = response
        .headers()
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok());
    assert_eq!(negotiated_protocol, Some("chat"));

    let first = client
        .next()
        .await
        .expect("missing first frame")
        .expect("websocket frame");
    assert_eq!(first.into_text().unwrap(), "protocol-ok");

    let _ = client.close(None).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn websocket_uses_custom_max_message_size() {
    let (mut client, _, handle) = spawn_router(
        Route::new(("/config".at(|upgrade: WebSocketUpgrade| async move {
            upgrade
                .max_message_size(Some(4))
                .on_upgrade(|mut socket| async move {
                    let limit = socket
                        .get_config()
                        .max_message_size
                        .map_or_else(|| "none".to_owned(), |value| value.to_string());
                    let _ = socket.send_text(limit).await;
                })
        }),)),
        "ws://localhost/config",
    )
    .await;

    let first = client
        .next()
        .await
        .expect("missing first frame")
        .expect("websocket frame");
    assert_eq!(first.into_text().unwrap(), "4");

    let _ = client.close(None).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn websocket_rejects_messages_exceeding_max_message_size() {
    use skyzen::websocket::WebSocketError;

    let (error_tx, error_rx) = tokio::sync::oneshot::channel::<WebSocketError>();
    let error_tx = Arc::new(std::sync::Mutex::new(Some(error_tx)));

    let (mut client, _, handle) = spawn_router(
        Route::new(("/limited".at(move |upgrade: WebSocketUpgrade| {
            let error_tx = Arc::clone(&error_tx);
            async move {
                upgrade
                    .max_message_size(Some(8))
                    .on_upgrade(move |mut socket| async move {
                        while let Some(result) = socket.next().await {
                            match result {
                                Ok(_) => {}
                                Err(error) => {
                                    let sender = error_tx.lock().unwrap().take();
                                    if let Some(tx) = sender {
                                        let _ = tx.send(error);
                                    }
                                    break;
                                }
                            }
                        }
                    })
            }
        }),)),
        "ws://localhost/limited",
    )
    .await;

    // Well within the limit: no error is produced.
    client
        .send(Message::text("ok"))
        .await
        .expect("send small message");

    // Exceeds the configured 8-byte cap: the server-side receive loop must surface an error.
    client
        .send(Message::text("x".repeat(64)))
        .await
        .expect("send oversized message");

    let error = error_rx.await.expect("server never observed an error");
    assert!(
        matches!(error, WebSocketError::Protocol(_)),
        "expected protocol error for oversized message, got: {error}"
    );

    let _ = client.close(None).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn websocket_json_convenience_methods() {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct TestMessage {
        value: i32,
        text: String,
    }

    let (mut client, _, handle) = spawn_router(
        Route::new(("/json".ws(|mut socket| async move {
            // Use recv_json() convenience method
            while let Some(Ok(msg)) = socket.recv_json::<TestMessage>().await {
                // Use send() convenience method for JSON
                let response = TestMessage {
                    value: msg.value * 2,
                    text: format!("Echo: {}", msg.text),
                };
                let _ = socket.send(&response).await;
            }
        }),)),
        "ws://localhost/json",
    )
    .await;

    // Send JSON message
    let send_msg = TestMessage {
        value: 42,
        text: "hello".to_string(),
    };
    let json_str = serde_json::to_string(&send_msg).unwrap();
    client
        .send(Message::text(json_str))
        .await
        .expect("send message");

    // Receive JSON response
    let reply = client
        .next()
        .await
        .expect("missing reply")
        .expect("websocket frame");
    let received: TestMessage = serde_json::from_str(&reply.into_text().unwrap()).unwrap();

    assert_eq!(received.value, 84);
    assert_eq!(received.text, "Echo: hello");

    let _ = client.close(None).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn websocket_binary_convenience_methods() {
    let (mut client, _, handle) = spawn_router(
        Route::new(("/binary".ws(|mut socket| async move {
            while let Some(Ok(message)) = socket.next().await {
                if let Some(data) = message.into_bytes() {
                    // Use send_binary() convenience method
                    let mut response = vec![0xFF];
                    response.extend_from_slice(&data);
                    let _ = socket.send_binary(response).await;
                }
            }
        }),)),
        "ws://localhost/binary",
    )
    .await;

    // Send binary message
    let test_data = vec![0x01, 0x02, 0x03, 0x04];
    client
        .send(Message::binary(test_data.clone()))
        .await
        .expect("send message");

    // Receive binary response
    let reply = client
        .next()
        .await
        .expect("missing reply")
        .expect("websocket frame");
    let received = reply.into_data();

    assert_eq!(received.len(), 5);
    assert_eq!(received[0], 0xFF);
    assert_eq!(&received[1..], &test_data[..]);

    let _ = client.close(None).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn websocket_session_failure_closes_with_internal_error() {
    let (mut client, _, handle) = spawn_router(
        Route::new(("/boom".ws(|mut socket| async move {
            // Take one message so the client is certainly connected, then fail the session the way
            // a handler would when a downstream call it depends on gives up.
            let _ = socket.next().await;
            Err::<(), _>(skyzen::Error::msg("the session gave up"))
        }),)),
        "ws://localhost/boom",
    )
    .await;

    client
        .send(Message::text("hello"))
        .await
        .expect("send message");

    let frame = client
        .next()
        .await
        .expect("the server closed without a frame")
        .expect("websocket frame");

    match frame {
        Message::Close(Some(frame)) => assert_eq!(
            u16::from(frame.code),
            skyzen::websocket::INTERNAL_ERROR,
            "a failed session must close with 1011, not {frame:?}"
        ),
        other => panic!("expected a close frame after a failed session, got {other:?}"),
    }

    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn websocket_session_success_leaves_the_connection_alone() {
    let (mut client, _, handle) = spawn_router(
        Route::new(("/done".ws(|mut socket| async move {
            socket.send_text("bye").await?;
            Ok::<_, skyzen::Error>(())
        }),)),
        "ws://localhost/done",
    )
    .await;

    let first = client
        .next()
        .await
        .expect("missing first frame")
        .expect("websocket frame");
    assert_eq!(first.into_text().unwrap(), "bye");

    // A session that ended cleanly gets no close frame from the framework: the socket is dropped
    // and the connection ends, exactly as it did before sessions could report failure. What
    // matters here is the negative — the client must never see the `1011` the failure path sends.
    if let Some(Ok(Message::Close(Some(frame)))) = client.next().await {
        assert_ne!(
            u16::from(frame.code),
            skyzen::websocket::INTERNAL_ERROR,
            "a successful session must not be closed as an internal error"
        );
    }

    handle.abort();
    let _ = handle.await;
}

// ── Durable Object hibernation sockets, natively ──
//
// The native simulator has to deliver websocket events to `DurableObject::websocket` and keep a
// registry of accepted sockets, or a room/relay object has no reachable code path off wasm32 and
// cannot be run under `skyzen dev` at all.

/// Steal the incoming request so it can be re-dispatched into a Durable Object.
///
/// A stub takes a whole `Request`, and a handler only ever sees extractors — so forwarding one
/// means an extractor that hands the request over intact. The extensions move rather than clone:
/// they carry hyper's upgrade handle, which is what makes the handshake inside the object possible.
struct ForwardedRequest(skyzen::Request);

impl skyzen_core::Extractor for ForwardedRequest {
    type Error = std::convert::Infallible;

    fn extract(
        request: &mut skyzen::Request,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send {
        let mut forwarded = skyzen::Request::new(skyzen::Body::empty());
        *forwarded.method_mut() = request.method().clone();
        *forwarded.uri_mut() = request.uri().clone();
        *forwarded.headers_mut() = request.headers().clone();
        *forwarded.extensions_mut() = std::mem::take(request.extensions_mut());
        std::future::ready(Ok(Self(forwarded)))
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[skyzen::durable_object]
struct RelayObject;

impl skyzen::durable::DurableObject for RelayObject {
    fn fetch(&mut self) -> skyzen::routing::Router {
        Route::new((
            "/relay".at(join_relay),
            "/authenticated".at(join_authenticated),
        ))
        .build()
    }

    // Relaying a frame is a synchronous fan-out over the connection registry, so the future is
    // ready on creation rather than an `async` block with nothing to await.
    fn websocket(
        &mut self,
        connection: &skyzen::durable::WebSocketConnection,
        event: skyzen::durable::WebSocketEvent,
        context: &skyzen::durable::DurableContext,
    ) -> impl std::future::Future<Output = Result<(), skyzen::durable::DurableObjectError>> + Send
    {
        std::future::ready(relay(connection, event, context))
    }
}

fn relay(
    connection: &skyzen::durable::WebSocketConnection,
    event: skyzen::durable::WebSocketEvent,
    context: &skyzen::durable::DurableContext,
) -> Result<(), skyzen::durable::DurableObjectError> {
    let skyzen::durable::WebSocketEvent::Message(skyzen::websocket::WebSocketMessage::Text(text)) =
        event
    else {
        return Ok(());
    };

    // The sender's own tags and the tagged fan-out both come out of the connection registry, so
    // one reply proves the socket was registered, tagged, and reachable by tag.
    let tags = connection.tags()?.join(",");
    let peers = context.connections().by_tag("relay")?;
    for peer in &peers {
        peer.send_text(&format!("{tags}/{}/{text}", peers.len()))?;
    }
    Ok(())
}

async fn join_relay() -> skyzen::durable::HibernationWebSocketUpgrade {
    skyzen::durable::HibernationWebSocketUpgrade::new().tag("relay")
}

#[tokio::test]
async fn durable_object_delivers_websocket_messages_natively() {
    let namespace = skyzen::durable::NativeDurableNamespace::<RelayObject>::new();

    let (mut client, _, handle) = spawn_router(
        Route::new((
            "/relay".at(move |ForwardedRequest(request): ForwardedRequest| {
                let namespace = namespace.clone();
                async move {
                    namespace
                        .get_by_name("lobby")
                        .expect("stub for the lobby object")
                        .fetch(request)
                        .await
                        .expect("durable object fetch")
                }
            }),
        )),
        "ws://localhost/relay",
    )
    .await;

    client
        .send(Message::text("hello"))
        .await
        .expect("send message");

    let reply = client
        .next()
        .await
        .expect("missing reply")
        .expect("websocket frame");
    assert_eq!(reply.into_text().unwrap(), "relay/1/hello");

    let _ = client.close(None).await;
    handle.abort();
    let _ = handle.await;
}

/// A hibernating socket that authenticates the way a browser has to.
///
/// The `WebSocket` constructor sends no custom headers, so the credential travels in the
/// subprotocol list — and RFC 6455 §4.1 makes the client fail the connection unless the server
/// echoes the accepted token back in the `101`.
async fn join_authenticated(
    offered: skyzen::websocket::RequestedSubprotocols,
) -> Result<skyzen::durable::HibernationWebSocketUpgrade, skyzen::websocket::WebSocketError> {
    const PREFIX: &str = "flyco.bearer.";

    let token = offered
        .iter()
        .find_map(|protocol| protocol.strip_prefix(PREFIX))
        .ok_or_else(|| {
            skyzen::websocket::WebSocketError::Protocol("no bearer subprotocol offered".to_owned())
        })?;
    assert_eq!(token, "s3cr3t", "the handler must see the client's token");

    let answer = offered
        .answer(|protocol| protocol.starts_with(PREFIX))
        .expect("the offered token is a valid header value");
    Ok(skyzen::durable::HibernationWebSocketUpgrade::new()
        .tag("relay")
        .protocol(answer))
}

#[tokio::test]
async fn durable_object_echoes_the_authenticating_subprotocol() {
    let mut request = "ws://localhost/authenticated"
        .into_client_request()
        .expect("build websocket request");
    request.headers_mut().append(
        SEC_WEBSOCKET_PROTOCOL,
        "flyco.bearer.s3cr3t"
            .parse()
            .expect("parse Sec-WebSocket-Protocol header"),
    );

    let namespace = skyzen::durable::NativeDurableNamespace::<RelayObject>::new();
    let (mut client, response, handle) = spawn_router(
        Route::new((
            "/authenticated".at(move |ForwardedRequest(request): ForwardedRequest| {
                let namespace = namespace.clone();
                async move {
                    namespace
                        .get_by_name("lobby")
                        .expect("stub for the lobby object")
                        .fetch(request)
                        .await
                        .expect("durable object fetch")
                }
            }),
        )),
        request,
    )
    .await;

    // Without this header the client would have failed the connection instead of opening it.
    assert_eq!(
        response
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok()),
        Some("flyco.bearer.s3cr3t")
    );

    // The socket is live: the object still relays through the registry it was tagged into.
    client
        .send(Message::text("hello"))
        .await
        .expect("send message");
    let reply = client
        .next()
        .await
        .expect("missing reply")
        .expect("websocket frame");
    assert_eq!(reply.into_text().unwrap(), "relay/1/hello");

    let _ = client.close(None).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn websocket_answers_a_credential_bearing_subprotocol_verbatim() {
    let mut request = "ws://localhost/token"
        .into_client_request()
        .expect("build websocket request");
    request.headers_mut().append(
        SEC_WEBSOCKET_PROTOCOL,
        "app.bearer.abc123"
            .parse()
            .expect("parse Sec-WebSocket-Protocol header"),
    );

    let (mut client, response, handle) = spawn_router(
        Route::new(("/token".at(|upgrade: WebSocketUpgrade| async move {
            // A fixed list of supported names cannot match a token, so the offer is read and
            // answered verbatim instead.
            let answer = upgrade
                .requested_protocols()
                .iter()
                .find(|protocol| protocol.starts_with("app.bearer."))
                .and_then(|protocol| skyzen::header::HeaderValue::from_str(protocol).ok())
                .expect("the client offered a bearer subprotocol");

            upgrade
                .protocol(answer)
                .on_upgrade(|mut socket| async move {
                    let _ = socket.send_text("authenticated").await;
                })
        }),)),
        request,
    )
    .await;

    assert_eq!(
        response
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok()),
        Some("app.bearer.abc123")
    );

    let first = client
        .next()
        .await
        .expect("missing first frame")
        .expect("websocket frame");
    assert_eq!(first.into_text().unwrap(), "authenticated");

    let _ = client.close(None).await;
    handle.abort();
    let _ = handle.await;
}
