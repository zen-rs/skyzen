//! WebSocket tests for the Skyzen framework.

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
