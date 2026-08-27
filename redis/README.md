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

Or from `REDIS_URL`, the way every other Skyzen backend reads its configuration:

```rust
use skyzen_redis::Redis;

let redis = Redis::from_env().await.unwrap();
```

Or from an existing connection manager:

```rust
use redis::aio::ConnectionManager;

let conn = ConnectionManager::new(client).await?;
let redis = Redis::from_connection_manager(conn);
```

### Atomic primitives

Beyond get/put/delete/list, this backend implements the whole `KeyValueStore` surface on Redis'
own commands: `put_if_absent` (`SET NX`) for distributed locks and idempotency keys,
`compare_and_swap` (a Lua script, since Redis has no compare-and-swap command and a GET/SET pair is
not atomic), `increment` (`INCRBY`) for rate-limit counters, `expire` (`PEXPIRE`) for sliding
sessions, and `exists` (`EXISTS`).

Redis Cluster and Sentinel are out of scope: this type speaks to a single endpoint, so a key that
hashes to another slot comes back as a `MOVED` error rather than being followed.

### Wiring into a Skyzen App

Wrap the `Redis` instance in `Kv` and inject it as middleware:

```rust
use skyzen::{
    extract::Path,
    routing::{CreateRouteNode, Route, Router},
    Result,
};
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

// `KvError` implements `HttpError`, so a bare `?` carries it — and its status — into the response.
// There is no `map_err` to write.
async fn get_cached(kv: Kv, Path(key): Path<String>) -> Result<String> {
    Ok(kv.get_text(&key).await?.unwrap_or_default())
}
```

## License

MIT or Apache-2.0, at your option.
