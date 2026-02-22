# Skyzen

[![crates.io](https://img.shields.io/crates/v/skyzen.svg)](https://crates.io/crates/skyzen)
[![doc.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen)
[![License](https://img.shields.io/crates/l/skyzen.svg)](LICENSE)
[![Coverage](https://img.shields.io/codecov/c/github/zen-rs/skyzen?logo=codecov)](https://app.codecov.io/gh/zen-rs/skyzen)

A fast, ergonomic HTTP framework for Rust that works everywhere — from native servers to WebAssembly edge platforms.

## Features

- **Write once, deploy everywhere** — The same handler code runs on native servers, Cloudflare Workers, AWS Lambda, and Azure Functions
- **Portable services** — Platform-agnostic abstractions for key-value stores, object storage, message queues, and databases
- **Extractor/Responder pattern** — Type-safe request parsing and response generation via function arguments and return types
- **Tree-based routing** — Fast, composable routing with path parameters, HTTP method matching, and nested routes
- **WebSocket support** — Unified WebSocket API across native (async-tungstenite) and WASM (WebSocketPair)
- **OpenAPI generation** — Automatic API documentation from annotated handlers
- **`#[skyzen::main]`** — One macro for both native (Tokio + Hyper + logging + graceful shutdown) and WASM (WinterCG `fetch` export)
- **Unified CLI** — `skyzen dev/deploy` for Cloudflare Workers, AWS Lambda, and Azure Functions

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

## Routing

Skyzen's routing system is built around `Route::new()` and intuitive path methods:

```rust
use skyzen::routing::{CreateRouteNode, Route, Router};

fn router() -> Router {
    Route::new((
        "/".at(|| async { "Home" }),
        "/users/{id}".at(|params: Params| async move {
            let id = params.get("id")?;
            Ok(format!("User: {id}"))
        }),
        "/posts".get(list_posts),
        "/posts".post(create_post),
        "/posts/{id}".put(update_post),
        "/posts/{id}".delete(delete_post),
    ))
    .build()
}
```

### WebSocket

```rust
use skyzen::routing::{CreateRouteNode, Route};
use skyzen::websocket::WebSocketUpgrade;

Route::new((
    "/ws".ws(|mut socket| async move {
        while let Some(Ok(message)) = socket.next().await {
            if let Some(text) = message.into_text() {
                let _ = socket.send_text(text).await;
            }
        }
    }),
))
```

WebSocket works on both native (via `async-tungstenite`) and WASM (via `WebSocketPair`).

## Platform Comparison

The same handler code runs unchanged across all platforms — only the service wiring differs:

| Service | Native | Cloudflare | AWS | Azure | Test |
|---------|--------|------------|-----|-------|------|
| Key-Value | [`skyzen-redis`](redis/) | `CfKv` | `DynamoKv` | `CosmosKv` | `InMemoryKv` |
| Object Storage | [`skyzen-s3`](s3/) | `CfR2` | `S3Storage` | `AzureBlob` | `InMemoryStorage` |
| Message Queue | — | `CfQueue` | `SqsQueue` | `ServiceBusQueue` | `InMemoryQueue` |
| SQL Database | SeaORM | `CfD1` / `CfDurableSqlite` | SeaORM | SeaORM | `InMemoryDb` |

See the [Services Guide](docs/services-guide.md) for how to write platform-agnostic handlers and switch between backends.

## Services Abstraction

Skyzen provides portable service abstractions through `skyzen-services`. The same handler code runs on any platform:

```rust
use skyzen_services::{Kv, Storage, Queue};

async fn handler(kv: Kv, storage: Storage) -> Result<Json<Data>> {
    let cached = kv.get_json::<Data>("cache:key").await?;
    let file = storage.get("assets/logo.png").await?;
    Ok(Json(cached.unwrap_or_default()))
}
```

Wire different backends depending on your deployment target:

```rust
// Native: Redis + S3
let kv = Kv::new(Redis::connect("redis://localhost:6379").await?);
let storage = Storage::new(S3Storage::from_env("my-bucket"));

// Testing: In-memory mocks
let kv = Kv::new(InMemoryKv::new());
let storage = Storage::new(InMemoryStorage::new());
```

## The `#[skyzen::main]` Macro

For HTTP servers, `#[skyzen::main]` provides:

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

Disable the default logger to configure your own:

```rust
#[skyzen::main(default_logger = false)]
async fn main() -> Router {
    tracing_subscriber::fmt().init();
    router()
}
```

### WASM Deployment

For serverless edge platforms, use a `lib` crate with `cdylib`:

```toml
[lib]
crate-type = ["cdylib", "rlib"]
```

```rust
#[skyzen::main]
fn app() -> Router {
    router()
}
```

On WASM targets, `#[skyzen::main]` exports a WinterCG-compatible `fetch` handler for Cloudflare Workers, Deno Deploy, and other edge runtimes.

See the [Deployment Guide](docs/deployment-guide.md) for full setup instructions.

## CLI

The `skyzen` CLI provides unified local emulation and deployment:

```sh
skyzen doctor                          # Check toolchain
skyzen dev --provider cloudflare       # Local Workers emulation
skyzen deploy --provider cloudflare    # Deploy to Workers
skyzen dev --provider aws              # SAM local API
skyzen deploy --provider aws           # SAM deploy
skyzen dev --provider azure            # Azure Functions local
skyzen deploy --provider azure         # Publish to Azure
```

Configure platforms via [`Skyzen.toml`](docs/skyzen-toml-reference.md).

## Custom Server

For advanced scenarios like embedding Skyzen or using a custom runtime:

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

## Workspace Crates

| Crate | Description | README |
|-------|-------------|--------|
| [`skyzen`](.) | Main framework — routing, middleware, extractors, responders, runtime | [README](README.md) |
| [`skyzen-core`](core/) | Foundational traits (`Extractor`, `Responder`, `Server`), `no_std` support | [README](core/README.md) |
| [`skyzen-hyper`](hyper/) | Hyper server backend | [README](hyper/README.md) |
| [`skyzen-macros`](macros/) | Procedural macros (`#[skyzen::main]`, `#[skyzen::openapi]`, etc.) | [README](macros/README.md) |
| [`skyzen-services`](services/) | Portable service traits and extractors (`Kv`, `Storage`, `Queue`, `Db`) | [README](services/README.md) |
| [`skyzen-test`](test/) | Mock services, `TestClient`, assertions, snapshot testing | [README](test/README.md) |
| [`skyzen-redis`](redis/) | Redis `KeyValueStore` implementation | [README](redis/README.md) |
| [`skyzen-s3`](s3/) | S3-compatible `ObjectStorage` implementation | [README](s3/README.md) |
| [`skyzen-cloudflare`](cloudflare/) | Cloudflare Workers implementations (KV, R2, Queues, D1, Durable Objects) | [README](cloudflare/README.md) |
| [`skyzen-aws`](aws/) | AWS implementations (DynamoDB, SQS, S3) | [README](aws/README.md) |
| [`skyzen-azure`](azure/) | Azure implementations (Cosmos DB, Blob Storage, Service Bus) | [README](azure/README.md) |
| [`skyzen-cli`](cli/) | Unified CLI for local emulation and deployment | [README](cli/README.md) |

## Guides

- [Using Portable Services](docs/services-guide.md) — How to write platform-agnostic handlers and switch backends
- [Testing with Skyzen](docs/testing-guide.md) — Mock services, TestClient, assertions, and snapshot testing
- [Deploying Skyzen Apps](docs/deployment-guide.md) — Native, Cloudflare Workers, AWS Lambda, Azure Functions
- [Skyzen.toml Reference](docs/skyzen-toml-reference.md) — Full configuration reference

## License

MIT or Apache-2.0, at your option.
