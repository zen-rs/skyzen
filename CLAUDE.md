# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Development Commands

```sh
# Format, lint, and test (standard workflow)
cargo fmt
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features

# Run a single test
cargo test --workspace --all-features test_name

# Run examples
cargo run --example native -- --port 3000
cargo run --example worker
cargo run --example openapi

# Build for wasm32 targets
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown --release
```

## Architecture Overview

Skyzen is a router-first HTTP framework targeting both native servers (Tokio + Hyper) and WebAssembly edge platforms. The codebase is a Cargo workspace with four crates:

### Crate Structure

- **`skyzen`** (root) - Main framework crate with routing, middleware, extractors, responders, and runtime helpers
- **`skyzen-core`** (`core/`) - Foundational traits (`Extractor`, `Responder`, `Server`) reusable by alternative runtimes. Supports `no_std` when the `std` feature is disabled
- **`skyzen-macros`** (`skyzen-macros/`) - Procedural macros: `#[skyzen::main]`, `#[skyzen::openapi]`, `#[skyzen::error]`, `#[derive(HttpError)]`
- **`skyzen-hyper`** (`hyper/`) - Hyper backend that implements the `Server` trait from `skyzen-core`

### Key Abstractions

1. **Extractor/Responder pattern** (`core/src/extract.rs`, `core/src/responder.rs`): Types that implement `Extractor` can be pulled from requests; types implementing `Responder` can write to responses. Tuples of extractors/responders compose automatically.

2. **Handler system** (`src/handler.rs`): Async functions with extractors as arguments and responders as return types are automatically converted into endpoints.

3. **Routing** (`src/routing/mod.rs`, `src/routing/router.rs`): Tree-based routing using `Route::new()` and the `CreateRouteNode` trait. Path literals gain builder methods (`.at()`, `.post()`, `.put()`, `.delete()`, `.ws()`, `.route()`).

4. **Dual-target runtime** (`src/runtime/`): The `#[skyzen::main]` macro generates either a native `fn main()` (with Tokio/Hyper, logging, CLI overrides) or a wasm `fetch` export for WinterCG platforms.

5. **OpenAPI integration** (`src/openapi/`): Handlers annotated with `#[skyzen::openapi]` contribute to auto-generated OpenAPI documentation. Uses `linkme` distributed slices for compile-time registration.

### Feature Flags

Default features: `json`, `form`, `multipart`, `sse`, `rt`, `openapi`, `ws`

- `rt` - Enables Tokio/Hyper runtime (native builds)
- `openapi` - OpenAPI schema generation (debug builds only)
- `ws` - Unified WebSocket support; pulls in the right backend per target (native via async-tungstenite, wasm via WebSocketPair)

#### WebSocket Platform Support

The WebSocket implementation provides a unified API across both native and WASM targets:

- **Native (tokio)**: Full WebSocket support via `async-tungstenite`, including custom ping/pong control
- **WASM (WinterCG)**: WebSocket support via custom FFI bindings to the WebSocketPair API, compatible with Cloudflare Workers and other WinterCG-compliant runtimes

**Platform differences:**
- WASM has a 1 MiB message size limit (platform imposed)
- WASM does not support custom ping/pong frame control
- Both platforms share the same convenience methods: `send()` for JSON, `send_text()` for text, `send_binary()` for binary, and `recv_json()` for JSON deserialization

### Workspace Lints

The workspace enforces strict Clippy lints including `pedantic` and `nursery` groups. All crates require `missing_docs` and `missing_debug_implementations` warnings.

### External Dependency

The framework depends on `http-kit` (git dependency from zen-rs/http-kit) which provides core HTTP types (`Request`, `Response`, `Body`, `Endpoint`, `Middleware`).
