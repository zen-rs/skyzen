# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/zen-rs/skyzen/compare/hyper/v0.3.0...hyper/v0.4.0) - 2026-11-12

### Added

- *(runtime)* [**breaking**] serve on http-kit 0.8 and zenwave 0.12 with operator io_uring settings
- *(core)* [**breaking**] build Upgraded from a connection's halves, spliceable on Linux

### Other

- *(runtime)* register the receive ring for incremental consumption

## [0.3.0](https://github.com/zen-rs/skyzen/compare/hyper/v0.2.1...hyper/v0.3.0) - 2026-11-11

### Added

- *(h1)* advertise an alternative service on every response
- *(runtime)* terminate TLS with http-kit-tls over owned-buffer streams
- *(runtime)* [**breaking**] serve unix sockets through http-kit-io, hyper included
- *(hyper)* [**breaking**] serve h1, h2 and upgrades on every http-kit-io driver, compio included
- [**breaking**] Server over http-kit-io listeners with per-thread !Send routers
- [**breaking**] bound on SendEndpoint where an endpoint's future crosses threads
- [**breaking**] adapt to http-kit's frame-based Body
- *(middleware)* RequestId, Trace and MatchedPath ([#162](https://github.com/zen-rs/skyzen/pull/162))
- *(runtime)* terminate TLS in the native server ([#144](https://github.com/zen-rs/skyzen/pull/144))

### Other

- *(deps)* bump http-kit to 0.6, http-kit-io 0.4, http-kit-h1 0.3, http-kit-h3 0.3 and http-kit-tls 0.2
- *(hyper)* test against http-kit-h3 0.2
- per-thread routers, the upgrade handover and the new Server call

## [0.2.1](https://github.com/zen-rs/skyzen/compare/hyper-v0.2.0...hyper-v0.2.1) - 2026-09-05

### Other

- updated the following local packages: skyzen-core

## [0.2.0](https://github.com/zen-rs/skyzen/compare/hyper-v0.1.1...hyper-v0.2.0) - 2026-08-27

### Added

- *(runtime)* [**breaking**] drain connections on Ctrl+C, install hyper's timer, add on_shutdown

### Fixed

- *(hyper)* sound reads, real timeouts, and Content-Length responses

### Other

- drop unused async from await-free trait impls
- [**breaking**] render and log endpoint errors through one shared pair of helpers
- cover error_response redaction and add hyper end-to-end tests

## [0.1.1](https://github.com/zen-rs/skyzen/compare/hyper-v0.1.0...hyper-v0.1.1) - 2025-12-10

### Other

- Add websocket tests and dev deps

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/hyper-v0.1.0) - 2025-12-09

### Added

- enhance WebSocket support with WASM integration and JSON message handling
- enhance WebSocket functionality with JSON support and split functionality
- add WebSocket echo server and health check endpoint
- enhance WebSocket testing with protocol negotiation and request handling
- migrate logging from `log` to `tracing` for improved observability
- enhance logging initialization options and improve handler trait definition
- enhance logging initialization with color-eyre and tracing, add examples for native and worker modes
- enhance dependency configurations, improve extractor and responder traits, and add logging initialization
- Implement static file serving with StaticDir
- enhance project metadata, improve dependency management, and refine HTTP server implementation
- update dependencies and improve HTTP response handling

### Fixed

- update dependencies in Cargo.toml and improve path sanitization in tests
- update dependencies, improve executor handling, and add hyper example
- update dependencies and improve WebSocket handling in native module
- run formatter
- Remove misuse of `DefaultExecutor`

### Other

- Fix typos
- update README to enhance clarity and structure, add examples for routing and WebSocket support
- enhance Hyper executor implementation and add HTTP/2 support in native runtime
- enhance async support in Cargo.toml and update WebSocket handling in native.rs
- update WebSocket send_text method to use ByteStr instead of String
- update dependencies and improve error handling
- remove AGENTS.md and add CLAUDE.md for updated project guidelines
- update dependencies and enhance WebSocket support
- improve code formatting and organization across multiple files
- Refactor routing and error handling in the Skyzen framework
- Refactor error handling and OpenAPI schema generation
- Reconstruct workspace & add lints
