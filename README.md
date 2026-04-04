# Skyzen

[![crates.io](https://img.shields.io/crates/v/skyzen.svg)](https://crates.io/crates/skyzen)
[![doc.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen)
[![License](https://img.shields.io/crates/l/skyzen.svg)](LICENSE)
[![Coverage](https://img.shields.io/codecov/c/github/zen-rs/skyzen?logo=codecov)](https://app.codecov.io/gh/zen-rs/skyzen)

A fast, ergonomic HTTP framework for Rust focused on native servers and Cloudflare-compatible edge/serverless platforms.

## Features

- **Portable core** — Write handlers against `Kv`, `Storage`, `Queue`, and `Db` instead of provider SDK types
- **Stable runtimes today** — Native servers and WinterCG/Cloudflare Workers share the same handler model
- **Provider extensions** — Opt into raw/provider-specific APIs such as `CfD1`, queue/scheduled event handlers, and Durable Object capabilities only when you need more than the portable minimum
- **Extractor/Responder pattern** — Type-safe request parsing and response generation via function arguments and return types
- **Tree-based routing** — Fast, composable routing with path parameters, HTTP method matching, and nested routes
- **WebSocket support** — Unified WebSocket API across native (async-tungstenite) and WASM (WebSocketPair)
- **OpenAPI generation** — Automatic API documentation from annotated handlers
- **`#[skyzen::main]`** — One macro for both native (Tokio + Hyper + logging + graceful shutdown) and WASM (WinterCG `fetch` export)
- **Unified CLI** — `skyzen new/dev/deploy` scaffolds projects, runs native watch/restart, and orchestrates Cloudflare-first deployment flows

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

Run with `cargo run` and open the address printed in the startup log, or pin a port explicitly with `cargo run -- --port 8787`.

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

Portable handlers run against the same capability wrappers. Native and Cloudflare automatic wiring are built in today; AWS and Azure provider crates remain available as infrastructure backends, but runtime parity for those targets is not part of the finished scope.

| Service | Native | Cloudflare | AWS | Azure | Test |
|---------|--------|------------|-----|-------|------|
| Key-Value | [`skyzen-redis`](redis/) | `CfKv` | `DynamoKv` | `CosmosKv` | `InMemoryKv` |
| Object Storage | [`skyzen-s3`](s3/) | `CfR2` | `S3Storage` | `AzureBlob` | `InMemoryStorage` |
| Message Queue | — | `CfQueue` | `SqsQueue` | `ServiceBusQueue` | `InMemoryQueue` |
| Portable SQL | `Db` via sqlx | `Db` via D1 | planned wiring | planned wiring | — |

Provider-specific escape hatches remain available when you need more than the portable minimum:

- Cloudflare raw SQL and stateful primitives: `CfD1`, `DurableKv`, `DurableDb`, `Alarm`, Durable Objects

See the [Services Guide](docs/services-guide.md) for how to write platform-agnostic handlers and switch between backends.
For per-object SQL state that runs on both native and Cloudflare, see the [Durable Object + SQL Guide](docs/durable-sql-guide.md).

## Services Abstraction

Skyzen provides portable capability wrappers through `skyzen-services`. Application code depends on those wrappers, not on provider SDK types:

```rust
use skyzen_services::{Db, Kv, Storage};

async fn handler(kv: Kv, storage: Storage, db: Db) -> Result<Json<Data>> {
    let cached = kv.get_json::<Data>("cache:key").await?;
    let file = storage.get("assets/logo.png").await?;
    let users = db
        .query("SELECT id, name FROM users")
        .fetch_all::<User>()
        .await?;
    Ok(Json(cached.unwrap_or_default()))
}
```

Wire different backends depending on your deployment target. The wrapper type stays the same:

```rust
// Native: Redis + S3
let kv = Kv::new(Redis::connect("redis://localhost:6379").await?);
let storage = Storage::new(S3Storage::from_env("my-bucket"));

// Cloudflare: Workers KV + R2
let kv = Kv::new(CfKv::from_env(&env, "CACHE")?);
let storage = Storage::new(CfR2::from_env(&env, "UPLOADS")?);

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

The `skyzen` CLI scaffolds projects, runs native watch/restart development, and orchestrates deployment:

```sh
skyzen new my-app --template api
skyzen new jobs-app --template serverless-events
skyzen new room-app --template durable-realtime
skyzen doctor                          # Check toolchain
skyzen dev                             # Native watch + restart
skyzen dev --provider cloudflare       # Wrangler-driven Cloudflare dev
skyzen deploy --provider cloudflare    # Deploy to Workers
skyzen dev --provider aws              # SAM local API
skyzen deploy --provider aws           # SAM deploy
skyzen dev --provider azure            # Azure Functions local
skyzen deploy --provider azure         # Publish to Azure
```

Configure platforms via [`Skyzen.toml`](docs/skyzen-toml-reference.md). For Cloudflare, Skyzen generates `.skyzen/gen/wrangler.toml` automatically; users do not hand-maintain `wrangler.toml`.

## Cloudflare Event Handlers

Skyzen also supports Cloudflare-specific queue and scheduled entrypoints:

```rust
#[cfg(target_arch = "wasm32")]
#[skyzen::queue]
async fn queue(
    batch: skyzen_cloudflare::CfQueueBatch,
    env: skyzen::runtime::wasm::Env,
    ctx: skyzen_cloudflare::CfQueueContext,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    batch.ack_all()?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
#[skyzen::scheduled]
async fn scheduled(
    event: skyzen_cloudflare::CfScheduledEvent,
    env: skyzen::runtime::wasm::Env,
    ctx: skyzen_cloudflare::CfScheduleContext,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    Ok(())
}
```

For stateful Cloudflare-specific workflows, use `#[skyzen::durable_object]` with the `DurableObject` trait.

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
| [`skyzen-aws`](aws/) | AWS infrastructure backends (DynamoDB, SQS, S3) | [README](aws/README.md) |
| [`skyzen-azure`](azure/) | Azure infrastructure backends (Cosmos DB, Blob Storage, Service Bus) | [README](azure/README.md) |
| [`skyzen-cli`](cli/) | Unified CLI for local emulation and deployment | [README](cli/README.md) |

## Guides

- [Using Portable Services](docs/services-guide.md) — How to write platform-agnostic handlers and switch backends
- [Testing with Skyzen](docs/testing-guide.md) — Mock services, TestClient, assertions, and snapshot testing
- [Deploying Skyzen Apps](docs/deployment-guide.md) — Native and Cloudflare-first deployment, plus AWS/Azure CLI orchestration notes
- [Skyzen.toml Reference](docs/skyzen-toml-reference.md) — Full configuration reference

## License

MIT or Apache-2.0, at your option.
