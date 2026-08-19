# Skyzen

[![crates.io](https://img.shields.io/crates/v/skyzen.svg)](https://crates.io/crates/skyzen)
[![doc.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen)
[![License](https://img.shields.io/crates/l/skyzen.svg)](#license)
[![Coverage](https://img.shields.io/codecov/c/github/zen-rs/skyzen?logo=codecov)](https://app.codecov.io/gh/zen-rs/skyzen)

An HTTP framework for Rust that compiles to a native Tokio server or to a
WinterCG `fetch` handler for Cloudflare Workers, from the same source.

Most Rust web frameworks assume a long-lived process with a thread pool. Most
edge SDKs assume a single-threaded WASM sandbox and hand you provider types
directly. Skyzen targets both: handlers are written against portable traits
(`Kv`, `Storage`, `Queue`, `Db`), `Send` bounds are applied conditionally per
target, and `#[skyzen::main]` expands to either a `fn main()` or a `fetch`
export depending on `target_arch`. When you need something a portable trait
can't express — D1's raw SQL, Durable Objects, alarms — the provider types are
still there.

## Quick start

```toml
[dependencies]
skyzen = "0.1"
```

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

`cargo run` starts the server and logs its address. `cargo run -- --port 8787`
pins the port.

## Routing

Routes are built from path literals. `CreateRouteNode` adds `.at()`, `.get()`,
`.post()`, `.put()`, `.delete()`, `.ws()`, and `.route()` to `&str`, so a route
tree is just a tuple of them:

```rust
use skyzen::routing::{CreateRouteNode, Params, Route, Router};

fn router() -> Router {
    Route::new((
        "/".at(home),
        "/users/{id}".at(|params: Params| async move {
            let id = params.get("id")?;
            Ok(format!("User: {id}"))
        }),
        "/posts".get(list_posts),
        "/posts".post(create_post),
        "/posts/{id}".put(update_post),
        "/posts/{id}".delete(delete_post),
        "/admin".route((
            "/stats".at(stats),
            "/flush".post(flush),
        )),
    ))
    .build()
}
```

Matching is done by [`matchit`](https://crates.io/crates/matchit)'s radix
tree, so a request is resolved by walking its own path rather than by testing
routes one at a time.

## Handlers

A handler is an async function. Its arguments are extractors, its return type
is a responder — nothing needs to be registered, and tuples of either compose
automatically.

```rust
use skyzen::{extract::Query, routing::Params, utils::Json, Result};

async fn create_user(Json(body): Json<CreateUser>) -> Result<Json<User>> {
    Ok(Json(User::insert(body).await?))
}

async fn search(Query(query): Query<SearchQuery>, params: Params) -> Json<Page> {
    // ...
}
```

`Json`, `Form`, `Query`, `Params`, `Multipart`, `State`, `BearerToken`, and
`ClientIp` are extractors out of the box. `String`, `&str`, `Json<T>`,
`Response`, and `Result<T>` are responders. Implement `Extractor` or
`Responder` for your own types.

## Errors

`#[skyzen::error]` writes the `Display`, `Error`, and `HttpError` impls, and
maps each variant to a status code. Messages interpolate fields the same way
`thiserror` does:

```rust
#[skyzen::error(message = "internal server error")]
enum ApiError {
    #[error("no user with id {0}", status = NOT_FOUND)]
    UserNotFound(u64),
    #[error("field {field} is invalid: {reason}", status = BAD_REQUEST)]
    Invalid { field: String, reason: String },
    #[error("upstream {service} timed out", status = GATEWAY_TIMEOUT)]
    Timeout { service: &'static str },
}
```

Returning `Err(ApiError::UserNotFound(7))` from a handler produces
`404 {"error":"no user with id 7"}`. Note that 5xx responses replace the
message with a generic `"Internal server error"` — a `Display` impl is for
your logs, and shouldn't leak into a client's error body by accident.

## Portable services

`skyzen-services` exposes four capability wrappers — `Kv`, `Storage`, `Queue`,
and `Db`. Handlers take them as extractors and never name a provider type:

```rust
use skyzen_services::{Db, Kv, Storage};

async fn handler(kv: Kv, storage: Storage, db: Db) -> Result<Json<Data>> {
    let cached = kv.get_json::<Data>("cache:key").await?;
    let logo = storage.get("assets/logo.png").await?;
    let users = db.query("SELECT id, name FROM users").fetch_all::<User>().await?;
    Ok(Json(cached.unwrap_or_default()))
}
```

The backend is chosen once, at wiring time:

```rust
// Native
let kv = Kv::new(Redis::connect("redis://localhost:6379").await?);
let storage = Storage::new(S3Storage::from_env("my-bucket"));

// Cloudflare
let kv = Kv::new(CfKv::from_env(&env, "CACHE")?);
let storage = Storage::new(CfR2::from_env(&env, "UPLOADS")?);

// Tests
let kv = Kv::new(InMemoryKv::new());
let storage = Storage::new(InMemoryStorage::new());
```

| Capability | Native | Cloudflare | AWS | Azure | Test |
|---|---|---|---|---|---|
| Key-value | [`skyzen-redis`](redis/) | `CfKv` | `DynamoKv` | `CosmosKv` | `InMemoryKv` |
| Object storage | [`skyzen-s3`](s3/) | `CfR2` | `S3Storage` | `AzureBlob` | `InMemoryStorage` |
| Message queue | — | `CfQueue` | `SqsQueue` | `ServiceBusQueue` | `InMemoryQueue` |
| SQL | `Db` via sqlx | `Db` via D1 | — | — | — |

Native and Cloudflare are wired automatically from `Skyzen.toml`. The AWS and
Azure crates are usable as backends, but you construct and inject them
yourself; there is no automatic wiring for them yet.

The provider-specific types (`CfD1`, `DurableKv`, `DurableDb`, `Alarm`) are
public and can be mixed into the same app. See the
[services guide](docs/services-guide.md) and the
[Durable Object + SQL guide](docs/durable-sql-guide.md).

## Running on the edge

The same router runs on Cloudflare Workers. Build a `cdylib` instead of a
binary:

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

On `wasm32`, `#[skyzen::main]` emits a WinterCG `fetch` export instead of a
`main`. Workers' queue and cron entrypoints have their own macros:

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

Stateful workloads use `#[skyzen::durable_object]` with the `DurableObject`
trait. Full setup is in the [deployment guide](docs/deployment-guide.md).

## WebSocket

```rust
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

Native uses `async-tungstenite`; WASM uses `WebSocketPair`. Two differences
worth knowing: WASM caps messages at 1 MiB, and it gives no control over
ping/pong frames.

## OpenAPI

Annotated handlers register themselves at compile time through `linkme`, so
the spec is assembled from the same signatures the router uses:

```rust
/// Fetch a user by id.
#[skyzen::openapi]
async fn get_user(params: Params) -> Result<Json<User>> {
    // ...
}

fn router() -> Router {
    Route::new(("/users/{id}".at(get_user),))
        .enable_api_doc() // served at /api-docs
        .build()
}
```

Doc comments become descriptions. Generation is gated on `debug_assertions`
and native targets, so neither release builds nor Workers bundles carry the
schema.

## `#[skyzen::main]`

On native targets the macro sets up a Tokio runtime and Hyper server, installs
a `tracing` subscriber that respects `RUST_LOG`, parses `--host` / `--port` /
`--listen`, and shuts down gracefully on `Ctrl+C`. To install your own
subscriber:

```rust
#[skyzen::main(default_logger = false)]
async fn main() -> Router {
    tracing_subscriber::fmt().init();
    router()
}
```

If you need to drive the server yourself — embedding Skyzen in a larger
process, or using a non-Tokio executor — `skyzen-hyper` implements the `Server`
trait directly:

```rust
use skyzen_hyper::Hyper;

Hyper.serve(
    my_executor,
    |error| tracing::error!(%error, "connection failed"),
    my_tcp_listener(),
    router().build(),
).await;
```

## CLI

```sh
skyzen new my-app --template api
skyzen new jobs-app --template serverless-events
skyzen new room-app --template durable-realtime
skyzen doctor                          # check toolchain
skyzen dev                             # native watch + restart
skyzen dev --provider cloudflare       # wrangler-driven dev
skyzen deploy --provider cloudflare
```

Bindings, queues, and databases are declared in
[`Skyzen.toml`](docs/skyzen-toml-reference.md). For Cloudflare the CLI
generates `.skyzen/gen/wrangler.toml` — don't hand-edit `wrangler.toml`.

## Crates

| Crate | What it is |
|---|---|
| [`skyzen`](.) | Routing, extractors, responders, middleware, runtime |
| [`skyzen-core`](core/) | `Extractor`, `Responder`, `Server` traits; `no_std`-capable |
| [`skyzen-hyper`](hyper/) | Hyper backend for the `Server` trait |
| [`skyzen-macros`](macros/) | `#[skyzen::main]`, `#[skyzen::openapi]`, `#[skyzen::error]`, and friends |
| [`skyzen-services`](services/) | `Kv`, `Storage`, `Queue`, `Db` and the traits behind them |
| [`skyzen-test`](test/) | In-memory services, `TestClient`, assertions |
| [`skyzen-redis`](redis/) | Redis `KeyValueStore` |
| [`skyzen-s3`](s3/) | S3-compatible `ObjectStorage` |
| [`skyzen-cloudflare`](cloudflare/) | Workers KV, R2, Queues, D1, Durable Objects (wasm32 only) |
| [`skyzen-aws`](aws/) | DynamoDB, SQS, S3 backends |
| [`skyzen-azure`](azure/) | Cosmos DB, Blob Storage, Service Bus backends |
| [`skyzen-cli`](cli/) | `skyzen new` / `dev` / `deploy` / `doctor` |

## Guides

- [Portable services](docs/services-guide.md)
- [Testing](docs/testing-guide.md)
- [Deployment](docs/deployment-guide.md)
- [`Skyzen.toml` reference](docs/skyzen-toml-reference.md)
- [Durable Objects + SQL](docs/durable-sql-guide.md)

Runnable examples live in [`examples/`](examples/).

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT),
at your option.
