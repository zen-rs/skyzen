# skyzen-core

[![crates.io](https://img.shields.io/crates/v/skyzen-core.svg)](https://crates.io/crates/skyzen-core)
[![docs.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen-core)
[![License](https://img.shields.io/crates/l/skyzen-core.svg)](../LICENSE)

Foundational traits for the Skyzen HTTP framework.

## Overview

`skyzen-core` defines the core abstractions that all Skyzen components build upon:

- **`Extractor`** — Types that can be extracted from an HTTP request (e.g. path params, JSON body, headers)
- **`Responder`** — Types that can be converted into an HTTP response (e.g. `String`, `Json<T>`, `StatusCode`)
- **`Server`** — Trait for HTTP server backends (implemented by `skyzen-hyper`)

This crate also re-exports foundational HTTP types from `http-kit`: `Request`, `Response`, `Body`, `Endpoint`, `Middleware`, `StatusCode`, and more.

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
