# skyzen-hyper

[![crates.io](https://img.shields.io/crates/v/skyzen-hyper.svg)](https://crates.io/crates/skyzen-hyper)
[![docs.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen-hyper)
[![License](https://img.shields.io/crates/l/skyzen-hyper.svg)](../LICENSE)

Hyper server backend for the Skyzen HTTP framework.

## Overview

`skyzen-hyper` provides the `Hyper` struct, which implements the `Server` trait from `skyzen-core`. It bridges Skyzen's `Endpoint` abstraction with Hyper's HTTP/1 and HTTP/2 server.

Most users don't need this crate directly — `#[skyzen::main]` sets up Hyper automatically. Use this crate when you want to:

- **Embed Skyzen** into an existing application with a custom async runtime
- **Use a non-Tokio executor** (e.g. `smol`, `async-std`)
- **Control connection handling** and error recovery directly

## Usage

Enable the `hyper` feature on the `skyzen` crate:

```toml
[dependencies]
skyzen = { version = "0.1", features = ["hyper"] }
```

Then use `Hyper.serve()` with an `http-kit-io` backend, a listener registered with it, a
thread-local executor and a shutdown signal. The endpoint never leaves the thread that serves it,
so it may be `!Send`:

```rust
use http_kit_io::{async_io::AsyncIo, Io};
use skyzen_core::Server;
use skyzen_hyper::Hyper;
use std::rc::Rc;

fn run(listener: std::net::TcpListener) -> std::io::Result<()> {
    let executor = Rc::new(async_executor::LocalExecutor::new());
    let listener = AsyncIo.tcp_listener(listener)?;
    let serve = Hyper.serve(
        AsyncIo,
        Rc::clone(&executor),
        listener,
        my_router().build(),
        shutdown_signal(),
    );
    futures_lite::future::block_on(executor.run(serve));
    Ok(())
}
```

See the [`embed_hyper.rs`](../examples/embed_hyper.rs) example for a complete working setup using `smol` as the async runtime.

## Key Types

| Type | Description |
|------|-------------|
| `Hyper` | `Server` implementation using Hyper HTTP/1 + HTTP/2 |
| `IntoService` | Adapter converting a Skyzen `Endpoint` into a Hyper `Service` |

## License

MIT or Apache-2.0, at your option.
