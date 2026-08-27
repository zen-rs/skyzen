//! WebSocket echo example demonstrating JSON and text message handling.
//!
//! This example shows:
//! - Text echo on `/ws`
//! - JSON echo on `/ws/json`
//! - Binary echo on `/ws/binary`
//!
//! Every session returns a [`Result`], which is what makes `?` usable on a send: a failed send is
//! reported instead of discarded, and the framework logs it and closes the connection with
//! [`websocket::INTERNAL_ERROR`](skyzen::websocket::INTERNAL_ERROR). The receive loops keep the
//! two ways a connection ends apart — a clean close and a transport failure look identical if you
//! write `while let Some(Ok(message))`, and only one of them is worth waking somebody for.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    websocket::{WebSocketMessage, WebSocketUpgrade},
    Responder, Result,
};

/// Example JSON message structure
#[derive(Serialize, Deserialize, Debug)]
struct ChatMessage {
    user: String,
    content: String,
}

/// Text echo handler - echoes back text messages
async fn websocket_echo(upgrade: WebSocketUpgrade) -> impl Responder {
    upgrade.on_upgrade(|mut socket| async move {
        while let Some(message) = socket.next().await {
            match message? {
                WebSocketMessage::Text(text) => socket.send_text(format!("echo:{text}")).await?,
                WebSocketMessage::Close => {
                    tracing::info!("client closed the text echo session");
                    break;
                }
                _ => {}
            }
        }
        Ok::<_, skyzen::Error>(())
    })
}

/// JSON echo handler - demonstrates `recv_json()` and `send()` convenience methods
async fn websocket_json(upgrade: WebSocketUpgrade) -> impl Responder {
    upgrade.on_upgrade(|mut socket| async move {
        // `recv_json` yields `None` on a clean close and `Some(Err(..))` on a failure, so `?`
        // separates "the client left" from "the connection broke".
        while let Some(message) = socket.recv_json::<ChatMessage>().await {
            let message = message?;
            tracing::info!(user = %message.user, content = %message.content, "received");

            socket
                .send(&ChatMessage {
                    user: "server".to_owned(),
                    content: format!("Echo: {}", message.content),
                })
                .await?;
        }
        tracing::info!("client closed the json session");
        Ok::<_, skyzen::Error>(())
    })
}

/// Binary echo handler - echoes back binary messages with a prefix byte
async fn websocket_binary(upgrade: WebSocketUpgrade) -> impl Responder {
    upgrade.on_upgrade(|mut socket| async move {
        while let Some(message) = socket.next().await {
            if let Some(data) = message?.into_bytes() {
                tracing::info!(bytes = data.len(), "received");

                let mut response = vec![0xFF];
                response.extend_from_slice(&data);
                socket.send_binary(response).await?;
            }
        }
        Ok::<_, skyzen::Error>(())
    })
}

/// The same session as `websocket_echo`, registered through the `.ws` shorthand instead of
/// extracting the upgrade by hand. It compiles unchanged for a Cloudflare Worker.
async fn shorthand_echo(mut socket: skyzen::websocket::WebSocket) -> Result<()> {
    while let Some(message) = socket.next().await {
        if let Some(text) = message?.into_text() {
            socket.send_text(text).await?;
        }
    }
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

fn router() -> Router {
    Route::new((
        "/ws".at(websocket_echo),
        "/ws/json".at(websocket_json),
        "/ws/binary".at(websocket_binary),
        "/ws/shorthand".ws(shorthand_echo),
        "/health".at(health),
    ))
    .build()
}

#[skyzen::main]
fn main() -> Router {
    router()
}
