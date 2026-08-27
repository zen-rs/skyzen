# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/zen-rs/skyzen/compare/v0.1.2...v0.2.0) - 2026-08-27

### Added

- *(manifest)* let an rds-data wiring name the cluster it addresses
- *(manifest)* wire Azure SQL from a [native.database] declaration
- *(aws)* build an RdsDataDb from named parts
- [**breaking**] reach Azure SQL through a portable T-SQL DbBackend
- *(cli)* install, preflight and report every declarative backend
- *(macros)* wire every cloud backend from [native.*] declarations
- *(azure)* read a Storage queue's signed URL from a named variable
- *(manifest)* model native wiring as per-backend tagged variants
- *(cli)* give the api template a database and a migration to apply
- *(cli)* report D1 migration status instead of refusing to
- *(services)* portable SQL migrations, embedded and applied on every backend
- *(deploy)* serve AWS Lambda and Azure Functions from the same binary
- *(aws)* add an RDS Data API DbBackend for Aurora
- *(runtime)* [**breaking**] drive `#[skyzen::queue]` natively, not only on Cloudflare
- *(cli)* add d1 migrations and forward runner arguments from dev
- *(cli)* [**breaking**] rebuild the command surface, the wrangler renderer and the dev loop
- *(manifest)* [**breaking**] one typed Skyzen.toml schema for the CLI and the macros
- *(ws)* [**breaking**] give websocket sessions an error channel and make `.ws` work on wasm32
- *(azure)* [**breaking**] page blob listings server-side, and own the account credentials
- *(azure)* [**breaking**] bind CosmosKv to a container that already exists
- *(azure)* [**breaking**] give Service Bus a consume side and add Azure Storage queues
- *(redis)* implement the atomic KV primitives and a from_env constructor
- *(s3)* stream, range, presign and multipart, and wire every put option
- *(aws)* [**breaking**] give SQS a consume side and FIFO support, DynamoDB atomics
- *(services)* [**breaking**] report which batch messages a queue send rejected
- *(cloudflare)* close the outbound cf, static assets and service-binding props gaps
- *(macros)* [**breaking**] export the email and tail Worker handlers
- *(durable)* [**breaking**] block concurrency, target jurisdictions, and let an object opt out of blob state
- *(cloudflare)* [**breaking**] reach the depth of KV, R2, Queues, D1 and service bindings
- *(runtime)* surface the Workers execution context and request.cf
- *(services)* [**breaking**] widen object storage options and give SQL an atomic batch
- *(test)* [**breaking**] carry all seven services through TestContext and open up the verbs
- *(macros)* [**breaking**] generate a named extractor per service, not just per database
- *(services)* [**breaking**] bind rich SQL types, bound single-row fetches, share one query builder
- *(services)* stream, range and presign objects instead of buffering them
- *(services)* [**breaking**] let queues delay a send and consume by pull
- *(services)* [**breaking**] give Kv the atomic primitives and paginate its listing
- *(sse,diagnostics)* keep SSE connections alive and explain unsatisfied trait bounds
- *(static-files)* [**breaking**] stream files and answer conditional and range requests
- *(routing)* [**breaking**] mount an already-built Router with nest()
- *(responder)* add Redirect, Html and TypedHeader, and re-export CookieJar
- *(extract)* add a typed Path<T> extractor and give path parameters real OpenAPI types
- *(extract)* [**breaking**] honour the body limit in Json/Form/Multipart and deserialize repeated keys
- *(core)* [**breaking**] enforce the request body limit and refuse a second body read
- *(runtime)* [**breaking**] drain connections on Ctrl+C, install hyper's timer, add on_shutdown
- *(routing)* [**breaking**] router layers, fallbacks and build-time wiring validation
- *(core)* [**breaking**] own the middleware trait, request-body limit and wiring requirements
- *(macros)* [**breaking**] emit `source()` from #[skyzen::error] and reject inert enum `message`
- *(services)* [**breaking**] give service errors an HttpError bridge and a source chain
- *(core)* [**breaking**] preserve HTTP status through `?` and make `Error` a real error type
- *(services)* add put_with options to ObjectStorage
- *(services)* add TTL support and richer error variants to KeyValueStore

### Fixed

- *(azure)* take the platform TLS path for tiberius
- *(handler)* stop generating an unreachable map_err for a handler with no arguments
- *(macros)* say what a mismatched backend does provide
- *(macros)* compare the embedded path in its escaped literal form for Windows
- *(cli)* assert the bundle location by path components, not a slashed string
- *(cli)* match bundle paths by component so the Azure tests pass on Windows
- *(ci)* satisfy machete and the Windows dead-code gate
- *(ci)* clear the two checks a current toolchain already fails
- *(azure)* give the test SAS credential its new expires_at field
- *(runtime)* re-export the cf module under test so its types are reachable
- *(runtime,azure)* survive fresh dependency resolves and workerd placeholder cf
- *(cli)* keep Skyzen.toml optional, and stop gating a build on runtime variables
- *(azure)* [**breaking**] refuse a Service Bus receive that carries no wait
- *(azure)* release a streamed upload's staged blocks when it gives up
- *(azure)* settle by sequence number, read 410 as a missing queue, block streamed uploads
- *(azure)* drop an unverifiable peek-lock cap and stop coupling `get` to an ETag
- *(docs)* correct three spellings the typos job flags
- *(openapi)* make the feature-off schema probe genuinely const
- *(routing)* keep a mounted router's path rooted when the prefix takes the slash
- *(openapi)* gate the Serialize import on the feature that uses it
- *(core)* fall back to the status number when it has no canonical reason
- *(examples,docs)* drop the `map_err` tax now that service errors carry a status
- *(extract)* [**breaking**] surface the real parse failure in Json, Form, Query and Multipart rejections
- *(cli)* force LF for assets embedded into generated projects
- *(ci)* repair lint, feature-matrix and workerd jobs
- *(cloudflare-admin)* honest crate description, pruned errors, into_result tests
- *(cloudflare)* durable persistence, SQL integer safety, D1 and header fixes
- *(cloudflare)* repair queue payload encoding, KV pagination/TTL, R2 metadata
- *(cli)* keep --dry-run read-only and stop leaking test temp dirs
- *(cli)* wire queue/scheduled worker exports and repair broken templates

### Other

- state where Azure SQL and a named RDS cluster are declared
- *(macros)* fix a typo in the Cosmos DB wiring comment
- document every backend a manifest can now declare
- correct four API shapes the guides had wrong
- *(examples)* take path parameters as `Path<T>`, not as strings
- make every documented claim match the shipped code
- *(services)* own the queue envelope codec in one place
- assert on emptiness in the form clippy asks for
- *(runtime)* make the consumer test driver future Send
- drop unused async from await-free trait impls
- the native queue cell contradicted the manifest that wires SQS natively
- *(aws)* say what a whole-chunk SQS batch failure loses
- *(ci)* exercise the execution context on the real workerd runtime
- *(services)* [**breaking**] delete MaybeSend and generate the object-safe service layer
- *(test)* encode form bodies with the same crate the framework decodes them with
- cover the new extractors end to end and make the docs match what ships
- *(middleware)* state that the body limit is advertised, not yet enforced
- describe the middleware trait, layers, fallbacks and wiring validation
- *(macros)* document the #[skyzen::error] attributes, including #[source]
- *(tests)* satisfy clippy::missing_const_for_fn in the new regression test
- [**breaking**] render and log endpoint errors through one shared pair of helpers
- Rewrite README and AGENTS.md
- Merge pull request #10 from zen-rs/main
- make the wasm modules clippy-clean
- drop stale CfDurableSqlite references after its removal
- Merge branch 'worktree-agent-a1962c2303a9353eb' into claude/skyzen-production-readiness-8kc2dg
- *(cloudflare)* dedupe JS-handle boilerplate and make the wasm build warning-free

## [0.1.2](https://github.com/zen-rs/skyzen/compare/v0.1.1...v0.1.2) - 2026-08-19

### Added

- *(macros)* interpolate fields in #[skyzen::error] messages

### Fixed

- *(license)* dual-license the root crate like the rest of the workspace
- *(cloudflare)* drop unnecessary mut on durable router binding

### Other

- correct README extractor list and OpenAPI gating
- add AGENTS.md and ignore .codex/
- rewrite README

## [0.1.1](https://github.com/zen-rs/skyzen/compare/v0.1.0...v0.1.1) - 2025-12-10

### Other

- Update description
- Add websocket tests and dev deps

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
