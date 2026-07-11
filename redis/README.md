# skyzen-redis

[![crates.io](https://img.shields.io/crates/v/skyzen-redis.svg)](https://crates.io/crates/skyzen-redis)
[![docs.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen-redis)
[![License](https://img.shields.io/crates/l/skyzen-redis.svg)](../LICENSE)

Redis implementation of `KeyValueStore` for the Skyzen framework.

## Overview

`skyzen-redis` provides a `Redis` struct that wraps `redis::aio::ConnectionManager` and implements the `KeyValueStore` trait from `skyzen-services`.

## Installation

```toml
[dependencies]
skyzen-redis = "0.1"
```

### Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `runtime-tokio` | Yes | Use Tokio as the async runtime |
| `runtime-smol` | No | Use Smol as the async runtime |

## Usage

### Connecting

```rust
use skyzen_redis::Redis;

let redis = Redis::connect("redis://127.0.0.1:6379").await.unwrap();
```

Or from an existing connection manager:

```rust
use redis::aio::ConnectionManager;

let conn = ConnectionManager::new(client).await?;
let redis = Redis::from_connection_manager(conn);
```

### Wiring into a Skyzen App

Wrap the `Redis` instance in `Kv` and inject it as middleware:

```rust
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen_redis::Redis;
use skyzen_services::Kv;

#[skyzen::main]
async fn main() -> Router {
    let redis = Redis::connect("redis://127.0.0.1:6379").await.unwrap();
    let kv = Kv::new(redis);

    Route::new((
        "/cache/{key}".get(get_cached),
    ))
    .with(kv)
    .build()
}

async fn get_cached(kv: Kv, params: skyzen::routing::Params) -> Result<String> {
    let key = params.get("key")?;
    let value = kv.get_text(key).await.map_err(|e| {
        skyzen_core::error::Error::internal(e.to_string())
    })?;
    Ok(value.unwrap_or_default())
}
```

## License

MIT or Apache-2.0, at your option.
