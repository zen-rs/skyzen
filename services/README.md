# skyzen-services

[![Crates.io](https://img.shields.io/crates/v/skyzen-services.svg)](https://crates.io/crates/skyzen-services)
[![Documentation](https://docs.rs/skyzen-services/badge.svg)](https://docs.rs/skyzen-services)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](https://github.com/zen-rs/skyzen/blob/main/LICENSE-MIT)

Portable service abstractions for the Skyzen HTTP framework.

## Overview

`skyzen-services` provides a set of portable capability wrappers for common backend services. Business logic depends on those wrappers instead of provider SDK types, so the same handler runs against Redis and Postgres on a server, Cloudflare KV and D1 at the edge, DynamoDB and SQS on AWS, Cosmos DB and Blob Storage on Azure, and in-process fakes in tests.

## Two-Trait Pattern

To enable type-erased dynamic dispatch while maintaining an ergonomic API for implementors, this crate employs a two-layer design:

1.  **Public Trait** (e.g., `KeyValueStore`): Ergonomic for backend implementors, using `impl Future` and standard bounds.
2.  **`*Obj` Trait**: Object-safe version using `BoxFuture` for dynamic dispatch. It and its blanket bridge are *generated* from the public trait by the `service_obj!` macro, so adding a method is a single-site change.
3.  **Bridge**: A blanket implementation that automatically adapts any `T: PublicTrait` to the object-safe version.
4.  **User-facing Wrapper** (e.g., `Kv`): A struct holding the boxed trait that implements both `Extractor` (a handler names it as an argument) and `Middleware` (attaching it injects it), allowing it to be wired with one `.with(kv)`.

## Service Traits

| Trait | Wrapper | Key Methods |
| :--- | :--- | :--- |
| `KeyValueStore` | `Kv` | `get`, `put`, `put_with_ttl`, `delete`, `exists`, paginated `list`; the atomics `put_if_absent`, `compare_and_swap`, `increment`, `expire`; and `get_json` / `get_text` / `put_json` / `list_all` on the wrapper |
| `ObjectStorage` | `Storage` | `get`, `put`, `put_with`, `delete`, `head`, paginated `list`, plus `get_stream`, `put_stream`, `get_range`, `presign_get`, `presign_put` |
| `MessageQueue` | `Queue` | produce with `send`, `send_batch`, `send_with`; consume with `receive`, `ack`, `nack`; and `send_json` / `send_json_batch` / `receive_json` on the wrapper |
| `DbBackend` | `Db` | `query(..).bind(..).fetch_one/fetch_all/fetch_optional/execute`, `begin` transactions, `execute_batch`, `migrate` |

A method a backend genuinely cannot provide keeps its `Unsupported` default and fails loudly, rather than being emulated with something racy.

Two more modules sit alongside the traits:

- `migrate` — the portable migration runner behind `Db::migrate`, which applies each `.sql` file and its bookkeeping row in one atomic batch and refuses to run when an applied file's checksum has changed.
- `queue::envelope` — the in-band encoding for queue transports that carry only text, shared by `skyzen-azure`'s Storage queue backend and the framework's Azure Functions integration so the two cannot drift.

`durable::{DurableKv, DurableDb}` cover object-scoped storage for Durable Objects and their native simulation.

### Database (`Db`)

`Db` is Skyzen's portable SQL wrapper. It follows a `sqlx`-style query-builder pattern, keeps parameters bound separately from the SQL string, and works across sqlx (PostgreSQL, MySQL, SQLite), Cloudflare D1 and the Aurora Data API. `begin` is a real transaction on sqlx and the Data API; D1 has none, so it returns `DbError::TransactionsUnsupported` there and `execute_batch` is D1's atomic unit.

## Quick Start

Handlers pull in services as extractors. The framework automatically retrieves the configured implementation from request extensions.

```rust
use skyzen_services::{Kv, Storage};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct UserProfile {
    name: String,
    avatar_url: String,
}

async fn update_profile(
    kv: Kv,
    storage: Storage,
    // Note: Json extractor is provided by the main skyzen crate
) -> Result<(), Box<dyn std::error::Error>> {
    let profile = UserProfile {
        name: "Alice".into(),
        avatar_url: "avatars/alice.png".into()
    };

    // Store metadata in KV
    kv.put_json("user:123", &profile).await?;

    // Check if avatar exists in object storage
    let _exists = storage.head(&profile.avatar_url).await?.is_some();

    Ok(())
}
```

## Platform Compatibility

Service traits require `Send` futures on **every** target, WASM included: the wrappers are handed
to handlers through `http::Extensions`, whose entries must be `Send + Sync` unconditionally, so a
`!Send` service future could never be stored there.

Single-threaded WASM backends still work, because portability is bought at the JS boundary rather
than in the trait: `skyzen-cloudflare` keeps each JS handle in a newtype with a contained
`unsafe impl Send + Sync` (sound because a Workers isolate never moves it across threads) and
awaits promises through `worker::send::IntoSendFuture`. See
[docs/services-guide.md](../docs/services-guide.md) for the full recipe.

## Feature Flags

### Runtime Selection
- `runtime-tokio-native-tls`: Use Tokio with `native-tls` for `sqlx`.
- `runtime-tokio-rustls`: Use Tokio with `rustls` for `sqlx`.
- `runtime-async-std-native-tls`: Use `async-std` with `native-tls` for `sqlx`.
- `runtime-async-std-rustls`: Use `async-std` with `rustls` for `sqlx`.

### Database Backends
- `postgres`, `mysql`, `sqlite` (all on by default): the sqlx drivers `Db::connect_*` builds on. Disable default features and pick one to trim compile times.

> **WASM Note**: these three are **inert** on `wasm32`, not an error — `sqlx` is a non-wasm dependency and every driver-backed path is additionally gated on `not(target_arch = "wasm32")`, so a crate compiled for both targets does not have to juggle features. At the edge, reach for `skyzen-cloudflare`'s D1 or Durable Object SQLite instead.

## Related Crates

These crates provide concrete implementations for the traits defined here:

- [`skyzen-redis`](../redis): Redis implementation for `KeyValueStore`.
- [`skyzen-s3`](../s3): S3-compatible implementation for `ObjectStorage`.
- [`skyzen-cloudflare`](../cloudflare): Cloudflare Workers (KV, R2, Queues, D1, Durable Objects).
- [`skyzen-aws`](../aws): AWS (DynamoDB, SQS, the Aurora Data API, and S3 re-exported).
- [`skyzen-azure`](../azure): Azure (Cosmos DB, Blob Storage, Service Bus, Storage queues).
- [`skyzen-test`](../test): In-memory mocks for unit testing.
