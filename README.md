# Skyzen

[![crates.io](https://img.shields.io/crates/v/skyzen.svg)](https://crates.io/crates/skyzen) 
[![doc.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen)
[![License](https://img.shields.io/crates/l/skyzen.svg)](LICENSE)
[![Coverage](https://img.shields.io/codecov/c/github/zen-rs/skyzen?logo=codecov)](https://app.codecov.io/gh/zen-rs/skyzen)


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

For production serverless deployments, use a `lib` crate (typically `cdylib`) instead of a binary target.

`Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib", "rlib"]
```

`src/lib.rs`:

```rust
#[skyzen::main]
fn app() -> Router {
    router()
}
```

Then build WebAssembly for edge platforms:

```sh
cargo build --target wasm32-unknown-unknown --release
```

On WASM targets, `#[skyzen::main]` exports a WinterCG-compatible `fetch` handler that works on Cloudflare Workers, Deno Deploy, and other edge runtimes.

### Unified Emulator & Deploy CLI

Skyzen now includes a unified CLI (`skyzen`) to run local emulators and deployments without writing provider-specific config files by hand.

Commands:

```sh
skyzen doctor
skyzen dev --provider cloudflare
skyzen deploy --provider cloudflare
skyzen dev --provider aws
skyzen deploy --provider aws
skyzen dev --provider azure
skyzen deploy --provider azure
```

You can also run it from source:

```sh
cargo run -p skyzen-cli -- dev --provider cloudflare --manifest ./Skyzen.toml
```

`Skyzen.toml` provider example:

```toml
[cloudflare]
name = "my-worker"
main = "dist/worker.js"
compatibility_date = "2025-02-01"
workers_dev = true

[[cloudflare.d1_databases]]
binding = "DB"
database_name = "app"
database_id = "your-d1-id"

[aws]
template = "template.yaml"
stack_name = "my-stack"
region = "us-east-1"
local_port = 3001

[azure]
project = "."
app_name = "my-function-app"
port = 7071
```

How providers map:
- Cloudflare: generates `.skyzen/gen/wrangler.toml`, then runs `wrangler dev/deploy`
- AWS: runs `sam local start-api` and `sam deploy`
- Azure: runs `func start` and `func azure functionapp publish`

### `Skyzen.toml` Datasource Sugar

`Skyzen.toml` is optional sugar for datasource declarations. Users can still wire everything manually in Rust.

```toml
[[datasource]]
name = "GlobalDb"
engine = "postgres"
strategy = "tcp"
url_from_env = "DATABASE_URL"
key_from_env = "DATABASE_TOKEN"
```

Generate strong-typed datasource code:

```rust
skyzen::import_config!();
```

`import_config!()` only generates code (types, `init()`, middleware, extractor). It does **not** execute initialization and does **not** auto-inject context.

When you use `#[skyzen::main]`, Skyzen will:
- expand `import_config!()` automatically
- call each generated `DatasourceType::init()` once during app startup
- install datasource middleware so handlers can extract typed datasource directly

If you do not use `#[skyzen::main]`, call `import_config!()` and wire middleware yourself.

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

## Services Abstraction

Skyzen provides portable service abstractions that let you write platform-agnostic business logic. The same handler code runs on Cloudflare Workers, AWS Lambda, Azure Functions, or a native server with Redis/SQL databases.

```rust
use skyzen_services::{Kv, Storage, Queue};

async fn handler(kv: Kv, storage: Storage) -> Result<Json<Data>> {
    // No knowledge of underlying implementation (Redis/CF KV/DynamoDB)
    let cached = kv.get_json::<Data>("cache:key").await?;
    let file = storage.get("assets/logo.png").await?;
    Ok(Json(cached.unwrap_or_default()))
}
```

**Service traits:**

| Trait | Wrapper | Purpose |
|-------|---------|---------|
| `KeyValueStore` | `Kv` | Key-value storage (get, put, delete, list) |
| `ObjectStorage` | `Storage` | Blob/object storage (get, put, delete, list, head) |
| `MessageQueue` | `Queue` | Message queues (send, send_batch) |
| SeaORM | `Db` | Database via SeaORM (re-exported) |

**Platform implementations:**

| Crate | Backends |
|-------|----------|
| `skyzen-redis` | Redis (KeyValueStore) |
| `skyzen-s3` | S3-compatible (ObjectStorage) |
| `skyzen-cloudflare` | CF KV, R2, Queues, D1, Durable Object SQLite |
| `skyzen-aws` | DynamoDB, SQS |
| `skyzen-azure` | Cosmos DB, Blob Storage, Service Bus |
| `skyzen-test` | In-memory mocks for all services (including SQLite test DB) |

Cloudflare SQL note:
- `CfD1`: D1 managed SQL database (Workers env binding)
- `CfDurableSqlite`: Durable Object `state.storage.sql` (inside Durable Object code)

### SQL Databases (SeaORM + `Db`)

Enable `skyzen-services` with one runtime feature plus a database backend feature:

```toml
[dependencies]
skyzen-services = { version = "0.1", features = ["runtime-tokio-rustls", "sqlite"] }
```

Database backend features:
- `sqlite` -> local SQLite (`sqlite::memory:` or file path)
- `postgres` -> PostgreSQL
- `mysql` -> MySQL/MariaDB

WASM restriction:
- Building `skyzen-services` with `sqlite` on `wasm32` is intentionally rejected at compile time.
- For WASM deployments, use cloud vendor database services (for Cloudflare: `CfD1` / `CfDurableSqlite`).

Native SQLite setup example:

```rust
use skyzen_services::{sea_orm::Database, Db};

async fn build_db() -> Result<Db, skyzen_services::sea_orm::DbErr> {
    // In-memory:
    let conn = Database::connect("sqlite::memory:").await?;

    // File-based local DB (example):
    // let conn = Database::connect("sqlite://skyzen.db?mode=rwc").await?;

    Ok(Db::new(conn))
}
```

### Message Queues (`Queue`)

Queue support is provided by `MessageQueue` + `Queue`, with current platform implementations:
- AWS SQS (`skyzen-aws`)
- Cloudflare Queues (`skyzen-cloudflare`)
- Azure Service Bus (`skyzen-azure`)
- In-memory queue for tests (`skyzen-test::mock::InMemoryQueue`)

The same handler API works across all queue backends:

```rust
use skyzen_services::Queue;

async fn enqueue(queue: Queue) -> Result<(), skyzen_services::QueueError> {
    queue.send_json(&serde_json::json!({ "event": "user.created" })).await?;
    Ok(())
}
```

### Cloudflare SQL Backends: D1 vs Durable Object SQLite

Use `skyzen-cloudflare` for Cloudflare-native SQL in WASM:

```rust
use skyzen_cloudflare::{CfD1, CfDurableSqlite};

// In a Worker request handler (from env bindings):
let d1 = CfD1::from_env(&env, "MY_D1")?;
let rows = d1.prepare("SELECT * FROM users")?.all().await?;

// Inside a Durable Object class (from `state`):
let do_sql = CfDurableSqlite::from_state(&state)?;
let cursor = do_sql.exec("SELECT * FROM users")?;
```

### Testing SQLite Locally (`skyzen-test::mock::InMemoryDb`)

`skyzen-test` now includes `InMemoryDb`, a SQLite memory-mode helper for integration tests.

```toml
[dev-dependencies]
skyzen-test = { version = "0.1", features = ["runtime-tokio-rustls"] }
```

```rust
use skyzen_test::mock::InMemoryDb;
use skyzen_services::sea_orm::ConnectionTrait;

async fn test_db_bootstrap() -> Result<(), skyzen_services::sea_orm::DbErr> {
    let db = InMemoryDb::with_schema(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
    )
    .await?;

    db.db()
        .execute_unprepared("INSERT INTO users (name) VALUES ('alice');")
        .await?;

    Ok(())
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
