# Skyzen

A fast, ergonomic HTTP framework for Rust that works everywhere - from native servers to WebAssembly edge platforms.

## Getting Started

```toml
[dependencies]
skyzen = "0.1"
```

The simplest Skyzen app:

```rust
use skyzen::routing::{CreateRouteNode, Route, Router};

#[skyzen::main]
fn main() -> Router {
    Route::new((
        "/".at(|| async { "Hello, World!" }),
        "/health".at(|| async { "OK" }),
    ))
    .build()
}
```

Run with `cargo run` and visit `http://127.0.0.1:8787`.

## Routing

Skyzen's routing system is built around `Route::new()` and intuitive path methods:

```rust
use skyzen::routing::{CreateRouteNode, Route, Router};

fn router() -> Router {
    Route::new((
        // Simple handlers
        "/".at(|| async { "Home" }),

        // Path parameters
        "/users/{id}".at(|params: Params| async move {
            let id = params.get("id")?;
            Ok(format!("User: {id}"))
        }),

        // HTTP methods
        "/posts".get(list_posts),
        "/posts".post(create_post),
        "/posts/{id}".put(update_post),
        "/posts/{id}".delete(delete_post),
    ))
    .build()
}
```

### WebSocket Support

Add WebSocket endpoints with the `.ws` convenience method:

```rust
use skyzen::routing::{CreateRouteNode, Route};
use skyzen::websocket::WebSocketUpgrade;

Route::new((
    // Simple echo server
    "/ws".ws(|mut socket| async move {
        while let Some(Ok(message)) = socket.next().await {
            if let Some(text) = message.into_text() {
                let _ = socket.send_text(text).await;
            }
        }
    }),

    // With protocol negotiation
    "/chat".at(|upgrade: WebSocketUpgrade| async move {
        upgrade
            .protocols(["chat", "superchat"])
            .on_upgrade(|mut socket| async move {
                // Handle the connection
            })
    }),
))
```

WebSocket works on both native (via `async-tungstenite`) and WASM (via `WebSocketPair`).

## The `#[skyzen::main]` Macro

For HTTP servers, `#[skyzen::main]` is the recommended way to start your app. It provides:

- **Pretty logging** with `tracing` (respects `RUST_LOG`)
- **Graceful shutdown** on `Ctrl+C`
- **CLI overrides** for host/port (`--port`, `--host`, `--listen`)
- **Tokio + Hyper runtime** configured and ready

```rust
#[skyzen::main]
fn main() -> Router {
    router()
}
```

Disable the default logger if you want to configure your own:

```rust
#[skyzen::main(default_logger = false)]
async fn main() -> Router {
    tracing_subscriber::fmt().init();
    router()
}
```

### WASM Deployment

The same code compiles to WebAssembly for edge platforms:

```sh
cargo build --target wasm32-unknown-unknown --release
```

On WASM targets, `#[skyzen::main]` exports a WinterCG-compatible `fetch` handler that works on Cloudflare Workers, Deno Deploy, and other edge runtimes.

## Custom Server Usage

For advanced scenarios like embedding Skyzen or using a custom runtime, implement the `Server` trait directly:

```rust
use skyzen::{Server, Endpoint};
use skyzen_hyper::Hyper;

async fn run_custom() {
    let router = router().build();
    let executor = MyExecutor::new();
    let connections = my_tcp_listener();

    Hyper.serve(
        executor,
        |error| eprintln!("Connection error: {error}"),
        connections,
        router,
    ).await;
}
```

The `Server` trait gives you full control over:
- Which executor to use (not tied to Tokio)
- Connection handling and error recovery
- Integration with existing infrastructure

## Extractors & Responders

Pull data from requests with extractors:

```rust
use skyzen::utils::Json;
use skyzen::routing::Params;

async fn create_user(
    params: Params,
    Json(body): Json<CreateUserRequest>,
) -> Result<Json<User>> {
    // params and body are automatically extracted
}
```

Return anything that implements `Responder`:

```rust
async fn handler() -> impl Responder {
    Json(data)  // or String, &str, Response, Result<T>, etc.
}
```

## OpenAPI Documentation

Generate API docs automatically:

```rust
#[skyzen::openapi]
async fn get_user(params: Params) -> Result<Json<User>> {
    // Handler implementation
}

fn router() -> Router {
    Route::new(("/users/{id}".at(get_user),))
        .enable_api_doc()  // Serves docs at /api-docs
        .build()
}
```

## License

MIT or Apache-2.0, at your option.
