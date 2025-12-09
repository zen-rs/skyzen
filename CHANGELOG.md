# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/v0.1.0) - 2025-12-09

### Added

- enhance WebSocket support with WASM integration and JSON message handling
- enhance WebSocket functionality with JSON support and split functionality
- implement native and wasm WebSocket support with shared types
- add WebSocket echo server and health check endpoint
- enhance WebSocket testing with protocol negotiation and request handling
- add logging for Skyzen application startup
- enhance OpenAPI support with deprecation handling and new examples
- migrate logging from `log` to `tracing` for improved observability
- add comprehensive article API with CRUD operations and OpenAPI documentation
- update OpenAPI schema handling to use BTreeMap for schema collectors and add RegisterSchemas trait
- enhance OpenAPI macro to support schema collectors and improve schema handling
- add Redoc API documentation endpoint and enhance route node handler chaining
- add support for #[ignore] and #[proxy] attributes in OpenAPI macro
- enhance OpenAPI macro to support schema generation and improve type handling
- introduce IgnoreOpenApi wrapper and update OpenApiSchema to return Option<RefOr<Schema>>
- add CI workflow for Rust with formatting, linting, and testing steps
- add OpenAPI support with new macros and documentation generation
- add multipart extractor for handling multipart/form-data requests
- enhance Extractor trait to require 'static bound for improved safety and consistency
- enhance logging initialization options and improve handler trait definition
- enhance logging initialization with color-eyre and tracing, add examples for native and worker modes
- enhance dependency configurations, improve extractor and responder traits, and add logging initialization
- Update wasm-bindgen CLI version, enhance worker configuration, and improve request/response handling
- Implement static file serving with StaticDir
- add repository guidelines and improve code structure, enhance error handling, and refine response handling
- enhance project metadata, improve dependency management, and refine HTTP server implementation
- update dependencies and improve HTTP response handling

### Fixed

- add feature flag configuration for hyper in embed_hyper example
- correct spelling of 'programmable' in router method
- make rt feature no-op on WASM targets
- handle WebSocket upgrade responses in WASM runtime
- update dependencies in Cargo.toml and improve path sanitization in tests
- enhance CI workflow with additional jobs for linting, coverage, and security audit
- add missing const allowance for register_responder_schemas_for function
- improve documentation for OpenAPI functions and reorganize WebSocket imports
- enhance feature flags usage, improve WebSocket handling, and refine error types
- update dependencies, improve executor handling, and add hyper example
- update dependencies and improve WebSocket handling in native module
- reorganize http_kit imports for better clarity and structure
- remove broken trybuild tests and ignore skyzen-core doctests
- ensure proper option wrapping for WebSocketConfig in from_raw_socket method
- remove unnecessary option wrapping for WebSocketConfig in from_raw_socket method
- update http-kit dependency to version 0.4
- run formatter
- Remove misuse of `DefaultExecutor`
- update OpenApi debug implementation to mask operations field
- update visibility of json module in responder
- update HttpError status methods to return Option<StatusCode>
- update code comments to remove ignore syntax for code blocks
- fix document test

### Other

- remove unused minimal Linux configuration and hyper feature flag from embed_hyper example
- add typos.toml to allow Flate (DEFLATE compression)
- Fix typos
- update CI and release workflows, add test workflow, and modify dependencies
- update code examples in error module to use skyzen_core::StatusCode
- fix code block formatting in error module examples
- update README to enhance clarity and structure, add examples for routing and WebSocket support
- enhance Hyper executor implementation and add HTTP/2 support in native runtime
- enhance async support in Cargo.toml and update WebSocket handling in native.rs
- rename WebSocket feature to 'ws' and update related documentation
- update feature flags for WebSocket support and add procedural macros for Skyzen framework
- update WebSocket message handling to use Option for text and binary messages
- update WebSocket send_text method to use ByteStr instead of String
- improve OpenAPI metadata handling and WebSocket message types
- update dependencies and improve error handling
- enhance authentication middleware and WebSocket configuration
- improve WebSocket upgrade handling and error reporting
- update dependencies and improve authentication middleware
- remove AGENTS.md and add CLAUDE.md for updated project guidelines
- update dependencies and enhance WebSocket support
- Refactor OpenAPI schema handling and remove unused code
- remove conditional compilation for OpenAPI responder implementations
- remove conditional compilation for OpenAPI schema collector functions
- Refactor OpenAPI integration and remove unused code
- remove unused binary target for test migration
- simplify error handling in openapi macro and update imports in builtins
- update OpenApi schema handling and improve type definitions
- improve code formatting and organization across multiple files
- add module documentation for form, json, multipart, and state utilities
- Refactor routing and error handling in the Skyzen framework
- update utoipa and utoipa-redoc dependencies; enhance OpenAPI redoc endpoint functionality
- Refactor error handling and OpenAPI schema generation
- Reconstruct workspace & add lints
- Refactor skyzen service and middleware for improved performance and clarity
- export some error types
- Client IP extractor implement
- Slimmer code
- Add `into_response` method for `Responder`
- routing improvement
- new http-kit api and new test framework
- move  to responder
- Handler shouldn't transform to `Middleware` directly
- run `cargo fmt` and fix document tests
- cargo fmt
- move the method of creating SSE channel
- write test for SSE
- export Sse
- move test to a single module
- make some features optional
- Server-Sent event implement
- dependency update
- move skyzen to a subfolder
- Hyper backend
- The initial implement of skyzen crate
- Core implement
