# skyzen-cloudflare

[![crates.io](https://img.shields.io/crates/v/skyzen-cloudflare.svg)](https://crates.io/crates/skyzen-cloudflare)
[![docs.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen-cloudflare)
[![License](https://img.shields.io/crates/l/skyzen-cloudflare.svg)](../LICENSE)

Cloudflare Workers service implementations for the Skyzen framework.

> **wasm32-only** — This crate must be built with `--target wasm32-unknown-unknown` and is excluded from default workspace builds.

## Overview

`skyzen-cloudflare` provides implementations of Skyzen's service traits for Cloudflare Workers platform APIs. All types use `wasm-bindgen` FFI to interact with the Workers runtime.

## Services

| Type | Implements | Cloudflare API |
|------|-----------|----------------|
| `CfKv` | `KeyValueStore` | [Workers KV](https://developers.cloudflare.com/kv/) |
| `CfR2` | `ObjectStorage` | [R2](https://developers.cloudflare.com/r2/) |
| `CfQueue` | `MessageQueue` | [Queues](https://developers.cloudflare.com/queues/) |
| `CfD1` | `DbBackend` + raw D1 API | [D1](https://developers.cloudflare.com/d1/) |
| `CfCache` | raw Cache API | [Cache API](https://developers.cloudflare.com/workers/runtime-apis/cache/) |
| `CfDurableSqlite` | — (direct SQL API) | [Durable Objects SQLite](https://developers.cloudflare.com/durable-objects/api/storage-api/) |

## Usage

All types are created from the Workers environment using binding names:

```rust
use skyzen_cloudflare::{CfCache, CfD1, CfDurableSqlite, CfKv, CfQueue, CfR2};

// From a Workers request handler (env is a JsValue):
let kv = CfKv::from_env(&env, "MY_KV")?;
let r2 = CfR2::from_env(&env, "MY_BUCKET")?;
let queue = CfQueue::from_env(&env, "MY_QUEUE")?;
let d1 = CfD1::from_env(&env, "DB")?;
let cache = CfCache::default();
```

### Key-Value (CfKv)

```rust
use skyzen_services::Kv;

let kv = Kv::new(CfKv::from_env(&env, "CACHE")?);
kv.put_json("user:1", &user).await?;
let user: Option<User> = kv.get_json("user:1").await?;
```

### Object Storage (CfR2)

```rust
use skyzen_services::Storage;

let storage = Storage::new(CfR2::from_env(&env, "UPLOADS")?);
storage.put("images/photo.png", image_bytes).await?;
let obj = storage.get("images/photo.png").await?;
```

### Message Queue (CfQueue)

```rust
use skyzen_services::Queue;

let queue = Queue::new(CfQueue::from_env(&env, "EVENTS")?);
queue.send_json(&json!({"event": "user.created"})).await?;
```

### D1 SQL Database

D1 can be used either through the portable `Db` wrapper or through the raw `CfD1` API:

```rust
use skyzen_services::Db;

let db = Db::new(CfD1::from_env(&env, "DB")?);
let rows = db
    .query("SELECT * FROM users WHERE id = ?")
    .bind(id)
    .fetch_all::<User>()
    .await?;
```

Raw D1 access remains available when you need provider-specific behavior:

```rust
let d1 = CfD1::from_env(&env, "DB")?;
let rows = d1
    .prepare("SELECT * FROM users WHERE id = ?")?
    .bind(&[id.into()])?
    .all()
    .await?;
```

### Cache API

Cloudflare's HTTP Cache API is available through `CfCache`:

```rust
use skyzen_cloudflare::CfCache;
use worker::Response;

let cache = CfCache::default();

cache
    .put_url(
        "https://example.com/data",
        Response::from_json(&serde_json::json!({"ok": true}))?,
    )
    .await?;

let cached = cache.get_url_json::<serde_json::Value>("https://example.com/data", false).await?;
```

`CfCache` methods return `Send` futures, and the high-level `get_*_bytes` / `get_*_text` / `get_*_json`
helpers are designed to be awaited directly inside Skyzen handlers without leaking a non-`Send`
`worker::Response` into application code.

### Durable Object SQLite

For SQL storage inside Durable Object classes using `state.storage.sql`:

```rust
let sql = CfDurableSqlite::from_state(&state)?;
let cursor = sql.exec("CREATE TABLE IF NOT EXISTS counter (id TEXT PRIMARY KEY, value INTEGER)")?;
```

## D1 vs Durable Object SQLite

| Feature | `CfD1` | `CfDurableSqlite` |
|---------|--------|-------------------|
| Scope | Global (Workers env binding) | Per-object instance |
| Access | Via `env` in request handler | Via `state` in DO class |
| Use case | Shared relational data | Per-entity state |

## Skyzen.toml Configuration

```toml
[cloudflare]
name = "my-worker"
main = "dist/worker.js"
compatibility_date = "2025-02-01"

[cloudflare.service.cache]
binding = "CACHE"

[cloudflare.database.main]
binding = "DB"

[[cloudflare.kv_namespaces]]
binding = "CACHE"
id = "abc123"

[[cloudflare.r2_buckets]]
binding = "UPLOADS"
bucket_name = "my-uploads"

[[cloudflare.d1_databases]]
binding = "DB"
database_name = "app"
database_id = "d1-id-here"

[[cloudflare.queues.producers]]
binding = "EVENTS"
queue = "my-events"

[[cloudflare.durable_objects.bindings]]
name = "STATE"
class_name = "AppState"

[[cloudflare.durable_objects.migrations]]
tag = "v1"
new_sqlite_classes = ["AppState"]
```

## Building

```sh
cargo clippy -p skyzen-cloudflare --target wasm32-unknown-unknown
```

## License

MIT or Apache-2.0, at your option.
