use futures_util::StreamExt;
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    websocket::{WebSocketMessage, WebSocketUpgrade},
    Responder,
};

async fn websocket_echo(upgrade: WebSocketUpgrade) -> impl Responder {
    upgrade.on_upgrade(|mut socket| async move {
        while let Some(Ok(message)) = socket.next().await {
            if let Ok(text) = message.into_text() {
                let _ = socket
                    .send(WebSocketMessage::text(format!("echo:{text}")))
                    .await;
            }
        }
    })
}

async fn health() -> &'static str {
    "ok"
}

fn router() -> Router {
    Route::new(("/ws".at(websocket_echo), "/health".at(health))).build()
}

#[skyzen::main]
fn main() -> Router {
    router()
}
