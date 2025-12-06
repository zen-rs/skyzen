//! WebSocket echo example demonstrating JSON and text message handling.
//!
//! This example shows:
//! - Text echo on `/ws`
//! - JSON echo on `/ws/json`
//! - Binary echo on `/ws/binary`

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    websocket::{WebSocketMessage, WebSocketUpgrade},
    Responder,
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
        while let Some(Ok(message)) = socket.next().await {
            match message {
                WebSocketMessage::Text(text) => {
                    let _ = socket.send_text(format!("echo:{text}")).await;
                }
                WebSocketMessage::Close => break,
                _ => {}
            }
        }
    })
}

/// JSON echo handler - demonstrates `recv_json()` and `send()` convenience methods
async fn websocket_json(upgrade: WebSocketUpgrade) -> impl Responder {
    upgrade.on_upgrade(|mut socket| async move {
        // Receive JSON messages using the convenient recv_json method
        while let Some(Ok(msg)) = socket.recv_json::<ChatMessage>().await {
            println!("Received from {}: {}", msg.user, msg.content);

            // Send JSON response using the convenient send method
            let response = ChatMessage {
                user: "server".to_string(),
                content: format!("Echo: {}", msg.content),
            };
            let _ = socket.send(&response).await;
        }
    })
}

/// Binary echo handler - echoes back binary messages with a prefix byte
async fn websocket_binary(upgrade: WebSocketUpgrade) -> impl Responder {
    upgrade.on_upgrade(|mut socket| async move {
        while let Some(Ok(message)) = socket.next().await {
            if let Some(data) = message.into_bytes() {
                println!("Received {} bytes", data.len());
                // Echo back with a prefix byte
                let mut response = vec![0xFF];
                response.extend_from_slice(&data);
                let _ = socket.send_binary(response).await;
            }
        }
    })
}

async fn health() -> &'static str {
    "ok"
}

fn router() -> Router {
    Route::new((
        "/ws".at(websocket_echo),
        "/ws/json".at(websocket_json),
        "/ws/binary".at(websocket_binary),
        "/health".at(health),
    ))
    .build()
}

#[skyzen::main]
fn main() -> Router {
    router()
}
