# skyzen-services

[![Crates.io](https://img.shields.io/crates/v/skyzen-services.svg)](https://crates.io/crates/skyzen-services)
[![Documentation](https://docs.rs/skyzen-services/badge.svg)](https://docs.rs/skyzen-services)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](https://github.com/zen-rs/skyzen/blob/main/LICENSE-MIT)

Portable service abstractions for the Skyzen HTTP framework.

## Overview

`skyzen-services` provides a set of portable capability wrappers for common backend services. Business logic depends on those wrappers instead of provider SDK types, so the same code can target native backends and Cloudflare edge backends without rewriting handlers.

## Two-Trait Pattern

To enable type-erased dynamic dispatch while maintaining an ergonomic API for implementors, this crate employs a two-layer design:

1.  **Public Trait** (e.g., `KeyValueStore`): Ergonomic for backend implementors, using `impl Future` and standard bounds.
2.  **Private `*Obj` Trait**: Object-safe version using `BoxFuture` for dynamic dispatch.
3.  **Bridge**: A blanket implementation that automatically adapts any `T: PublicTrait` to the object-safe version.
4.  **User-facing Wrapper** (e.g., `Kv`): A struct holding the boxed trait that implements `Extractor`, allowing it to be injected directly into handlers.

## Service Traits

| Trait | Wrapper | Key Methods |
| :--- | :--- | :--- |
| `KeyValueStore` | `Kv` | `get`, `put`, `delete`, `list` + `get_json`, `get_text`, `put_json` |
| `ObjectStorage` | `Storage` | `get`, `put`, `delete`, `list`, `head` |
| `MessageQueue` | `Queue` | `send`, `send_batch` + `send_json`, `send_json_batch` |
| `DbBackend` | `Db` | `query(...).bind(...).fetch_*`, `execute` |

### Database (`Db`)

`Db` is Skyzen's portable SQL wrapper. It follows a `sqlx`-style query-builder pattern, keeps parameters bound separately from the SQL string, and works across native backends and Cloudflare D1.

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

### Database Backends
- `postgres`, `mysql`, `sqlite`: Compatibility feature flags for SQL backends.

> **WASM Note**: The `sqlite` feature is rejected at compile-time on `wasm32` targets. For edge environments like Cloudflare, use vendor-specific databases via `skyzen-cloudflare` (e.g., D1 or Durable Object SQLite).

## Related Crates

These crates provide concrete implementations for the traits defined here:

- [`skyzen-redis`](../redis): Redis implementation for `KeyValueStore`.
- [`skyzen-s3`](../s3): S3-compatible implementation for `ObjectStorage`.
- [`skyzen-cloudflare`](../cloudflare): Cloudflare Workers (KV, R2, Queues, D1).
- [`skyzen-aws`](../aws): AWS (DynamoDB, SQS).
- [`skyzen-azure`](../azure): Azure (Cosmos DB, Blob Storage, Service Bus).
- [`skyzen-test`](../test): In-memory mocks for unit testing.
