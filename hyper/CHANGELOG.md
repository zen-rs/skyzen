# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
