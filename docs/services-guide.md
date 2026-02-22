# Using Portable Services

Skyzen's services abstraction lets you write platform-agnostic business logic that runs unchanged on Cloudflare Workers, AWS Lambda, Azure Functions, or a native server with Redis and S3.

## Mental Model

Every Skyzen service follows a three-layer architecture:

```
Trait (KeyValueStore)       ← Implement this for your backend
    ↓
Wrapper (Kv)                ← Type-erased, object-safe, Clone
    ↓
Extractor (handler arg)     ← Pulled from request extensions automatically
```

1. **Trait** — A public, ergonomic trait (e.g. `KeyValueStore`) that backend crates implement. Returns `impl Future`, so it's *not* object-safe but easy to write.
2. **Wrapper** — A struct (e.g. `Kv`) that holds `Box<dyn KeyValueStoreObj>`. An internal bridge trait converts any `KeyValueStore` into the object-safe form automatically.
3. **Extractor** — The wrapper implements `Extractor`, so handlers receive it as a function argument. No manual lookup required.

## Service Traits

| Trait | Wrapper | Operations |
|-------|---------|------------|
| `KeyValueStore` | `Kv` | `get`, `put`, `delete`, `list` + `get_json`, `get_text`, `put_json` |
| `ObjectStorage` | `Storage` | `get`, `put`, `delete`, `list`, `head` |
| `MessageQueue` | `Queue` | `send`, `send_batch` + `send_json`, `send_json_batch` |

`Db` wraps a `sea_orm::DatabaseConnection` and implements `Extractor` for SQL database access (native-only). It is not a Skyzen service trait — it re-exports SeaORM directly.

## Writing a Handler

Handlers declare service wrappers as arguments — Skyzen extracts them from request extensions:

```rust
use skyzen_services::{Kv, Storage};
use skyzen::utils::Json;

async fn upload(kv: Kv, storage: Storage, Json(body): Json<UploadRequest>) -> Result<&'static str> {
    // Store metadata in KV
    kv.put_json(&format!("file:{}", body.name), &body.metadata).await?;

    // Store file bytes in object storage
    storage.put(&body.name, body.data).await?;

    Ok("uploaded")
}
```

This handler works identically whether `Kv` is backed by Redis, DynamoDB, Cloudflare KV, or an in-memory mock.

## Wiring Services Manually

Before a handler can extract `Kv` or `Storage`, you must inject the concrete implementation as middleware. Here's how to wire Redis + S3 on native:

```rust
use skyzen::routing::{CreateRouteNode, Route};
use skyzen_redis::Redis;
use skyzen_s3::S3Storage;
use skyzen_services::{Kv, Storage};

#[skyzen::main]
async fn main() -> Router {
    // Create concrete backends
    let redis = Redis::connect("redis://127.0.0.1:6379").await.unwrap();
    let s3 = S3Storage::from_env("my-bucket");

    // Wrap in type-erased service wrappers
    let kv = Kv::new(redis);
    let storage = Storage::new(s3);

    Route::new((
        "/upload".post(upload),
    ))
    .with(kv)        // Injects Kv into all request extensions
    .with(storage)   // Injects Storage into all request extensions
    .build()
}
```

## Wiring via `Skyzen.toml`

For projects using the Skyzen CLI, `Skyzen.toml` can declare datasources and `import_config!()` generates the wiring code:

```toml
[[datasource]]
name = "MainDb"
engine = "postgres"
strategy = "tcp"
url_from_env = "DATABASE_URL"
```

```rust
skyzen::import_config!();

#[skyzen::main]
fn main() -> Router {
    // import_config!() is expanded automatically by #[skyzen::main].
    // Datasources are initialized and injected as middleware.
    Route::new((
        "/users".get(list_users),
    ))
    .build()
}
```

When you use `#[skyzen::main]`, Skyzen will:
- Expand `import_config!()` automatically
- Call each generated datasource's `init()` during startup
- Install datasource middleware so handlers can extract typed datasources

If you don't use `#[skyzen::main]`, call `import_config!()` and wire middleware yourself.

## Platform Switching

The same handler code runs on any platform — you only change the wiring:

### Native (Redis + S3)

```rust
let kv = Kv::new(Redis::connect("redis://localhost:6379").await?);
let storage = Storage::new(S3Storage::from_env("my-bucket"));
```

### AWS (DynamoDB + S3)

```rust
let kv = Kv::new(DynamoKv::from_env("my-table").await);
let storage = Storage::new(S3Storage::from_env("my-bucket"));
```

### Azure (Cosmos DB + Blob)

```rust
let kv = Kv::new(CosmosKv::new(client, "my-db", "my-container"));
let storage = Storage::new(AzureBlob::new(client, "my-container"));
```

### Cloudflare Workers (KV + R2)

On WASM targets, services are created from the Workers environment bindings inside the request handler or startup:

```rust
let kv = Kv::new(CfKv::from_env(&env, "MY_KV")?);
let storage = Storage::new(CfR2::from_env(&env, "MY_R2")?);
```

### Testing (In-Memory)

```rust
let kv = Kv::new(InMemoryKv::new());
let storage = Storage::new(InMemoryStorage::new());
```

Notice the handler function (`upload`) never changes. Only the one-line construction of each backend differs.

## Platform Implementations

| Service | Native | Cloudflare | AWS | Azure | Test |
|---------|--------|------------|-----|-------|------|
| Key-Value | [`skyzen-redis`](../redis/) | `CfKv` | `DynamoKv` | `CosmosKv` | `InMemoryKv` |
| Object Storage | [`skyzen-s3`](../s3/) | `CfR2` | `S3Storage` | `AzureBlob` | `InMemoryStorage` |
| Message Queue | — | `CfQueue` | `SqsQueue` | `ServiceBusQueue` | `InMemoryQueue` |
| SQL Database | SeaORM | `CfD1` / `CfDurableSqlite` | SeaORM | SeaORM | `InMemoryDb` |

## The `MaybeSend` Pattern

Skyzen services must work on both native (where futures need `Send` for multi-threaded runtimes) and WASM (where `Send` is not required and often impossible). The `MaybeSend` trait solves this:

```rust
// On native (not wasm32):
pub trait MaybeSend: Send {}
impl<T: Send> MaybeSend for T {}

// On wasm32:
pub trait MaybeSend {}
impl<T> MaybeSend for T {}
```

Service traits use `MaybeSend` as a bound instead of `Send`. This means:
- On native, futures must be `Send` (as expected for Tokio/smol)
- On WASM, any future works (no `Send` requirement)

The same trait definition compiles on both targets without `#[cfg]` in user code.

## Database Access

SQL databases use SeaORM through the `Db` wrapper (native-only):

```rust
use skyzen_services::{sea_orm::Database, Db};

let conn = Database::connect("sqlite::memory:").await?;
let db = Db::new(conn);
```

Enable the required runtime and database features:

```toml
[dependencies]
skyzen-services = { version = "0.1", features = ["runtime-tokio-rustls", "postgres"] }
```

Available database backends: `postgres`, `mysql`, `sqlite`.

**WASM restriction**: The `sqlite` feature is rejected at compile time on `wasm32` targets. For WASM deployments, use cloud vendor SQL services (`CfD1`, `CfDurableSqlite`).
