# skyzen-core

[![crates.io](https://img.shields.io/crates/v/skyzen-core.svg)](https://crates.io/crates/skyzen-core)
[![docs.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen-core)
[![License](https://img.shields.io/crates/l/skyzen-core.svg)](../LICENSE)

Foundational traits for the Skyzen HTTP framework.

## Overview

`skyzen-core` defines the core abstractions that all Skyzen components build upon:

- **`Extractor`** — Types that extract themselves from a `&mut Request` (a JSON body, a header, the raw bytes). Tuples of extractors are extractors, up to arity 15.
- **`Responder`** — Types that write themselves into a `&mut Response` (`String`, `StatusCode`, `HeaderMap`, tuples of the above)
- **`Middleware`** — Skyzen's own middleware trait, taking **`&self`** so a middleware value is stored once and shared across requests, with `Next::run` continuing the chain. It lives here rather than in `skyzen` so `skyzen-services` can implement it without depending on the framework.
- **`Server`** — Trait for HTTP server backends (implemented by `skyzen-hyper`)
- **The error vocabulary** — `Error`, `HttpError`, `Result`, `ResultExt` and `Context`, including the policy that redacts a 5xx body while keeping the whole `source()` chain in the log.
- **The body rules** — `RequestBodyLimit` (2 MiB by default, enforced from `Content-Length` and mid-stream) and `BodyConsumed`, which makes a second body-consuming extractor a loud error rather than a silent empty body.

This crate also re-exports foundational HTTP types from `http-kit`: `Request`, `Response`, `Body`, `Endpoint`, `StatusCode`, and more.

## `no_std` Support

`skyzen-core` supports `no_std` environments when the default `std` feature is disabled:

```toml
[dependencies]
skyzen-core = { version = "0.1", default-features = false }
```

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `std` | Yes | Standard library support |
| `openapi` | No | OpenAPI schema generation (requires `std`) |

## Usage

Most users should use the `skyzen` crate directly, which re-exports everything from `skyzen-core`. This crate is primarily useful for:

- **Alternative runtime implementors** — Implement the `Server` trait for a custom HTTP server
- **Library authors** — Build extensions that only depend on core traits, not the full framework
- **`no_std` environments** — Use extractors and responders in constrained contexts

```rust
use skyzen_core::{Extractor, Responder, Server};
use skyzen_core::{Request, Response, Body, Endpoint};
```

## License

MIT or Apache-2.0, at your option.
