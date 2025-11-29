use async_tungstenite::{client_async, tokio::TokioAdapter, tungstenite::Message, WebSocketStream};
use futures_util::{SinkExt, StreamExt};
use skyzen::{
    routing::{CreateRouteNode, Route},
    websocket::{WebSocketMessage, WebSocketUpgrade},
};
use tokio::io::duplex;

async fn spawn_router(
    router: Route,
    path: &str,
) -> (
    WebSocketStream<TokioAdapter<tokio::io::DuplexStream>>,
    tokio::task::JoinHandle<()>,
) {
    let router = router.build();
    let (client_stream, server_stream) = duplex(1024);
    let handle = tokio::spawn(async move {
        let io = hyper_util::rt::TokioIo::new(server_stream);
        let service = skyzen_hyper::use_hyper(router);
        let executor = hyper_util::rt::TokioExecutor::new();

        if let Err(error) = hyper_util::server::conn::auto::Builder::new(executor)
            .serve_connection_with_upgrades(io, service)
            .await
        {
            panic!("websocket server failure: {error}");
        }
    });

    let (client, _) = client_async(
        format!("ws://localhost{path}"),
        TokioAdapter::new(client_stream),
    )
    .await
    .expect("connect to websocket server");

    (client, handle)
}

#[tokio::test]
async fn websocket_roundtrip_over_hyper() {
    let (mut client, handle) = spawn_router(
        Route::new(("/ws".ws(|mut socket| async move {
            while let Some(Ok(message)) = socket.next().await {
                if let Ok(text) = message.into_text() {
                    let _ = socket.send(WebSocketMessage::text(text)).await;
                }
            }
        }),)),
        "/ws",
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
async fn websocket_uses_custom_max_message_size() {
    let (mut client, handle) = spawn_router(
        Route::new(("/config".at(|upgrade: WebSocketUpgrade| async move {
            upgrade
                .max_message_size(Some(4))
                .on_upgrade(|mut socket| async move {
                    let limit = socket
                        .get_config()
                        .max_message_size
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_owned());
                    let _ = socket.send(WebSocketMessage::text(limit)).await;
                })
        }),)),
        "/config",
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
