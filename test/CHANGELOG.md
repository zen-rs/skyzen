# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0](https://github.com/zen-rs/skyzen/compare/test/v0.4.0...test/v0.5.0) - 2026-11-12

### Added

- *(runtime)* [**breaking**] serve on http-kit 0.8 and zenwave 0.12 with operator io_uring settings

### Other

- *(runtime)* register the receive ring for incremental consumption

## [0.4.0](https://github.com/zen-rs/skyzen/compare/test/v0.3.2...test/v0.4.0) - 2026-11-11

### Added

- *(test)* record the thread each connection is dialed on
- *(test)* add LoopbackServer for end-to-end backend tests
- [**breaking**] bound on SendEndpoint where an endpoint's future crosses threads
- *(middleware)* RateLimit — fixed-window counter over portable KV ([#169](https://github.com/zen-rs/skyzen/pull/169))
- *(middleware)* RequestId, Trace and MatchedPath ([#162](https://github.com/zen-rs/skyzen/pull/162))

### Fixed

- *(services)* [**breaking**] run sqlx on its runtime-neutral smol runtime

## [0.3.2](https://github.com/zen-rs/skyzen/compare/skyzen-test-v0.3.1...skyzen-test-v0.3.2) - 2026-11-05

### Fixed

- reach database drivers, sqlx runtimes and TLS only through features

## [0.3.1](https://github.com/zen-rs/skyzen/compare/skyzen-test-v0.3.0...skyzen-test-v0.3.1) - 2026-11-05

### Other

- updated the following local packages: skyzen-services

## [0.3.0](https://github.com/zen-rs/skyzen/compare/skyzen-test-v0.2.1...skyzen-test-v0.3.0) - 2026-10-27

### Added

- [**breaking**] adopt sqlx 0.9 and stop forcing sqlx backends on leaf crates ([#99](https://github.com/zen-rs/skyzen/pull/99))

## [0.2.1](https://github.com/zen-rs/skyzen/compare/skyzen-test-v0.2.0...skyzen-test-v0.2.1) - 2026-09-05

### Other

- updated the following local packages: skyzen-core, skyzen-services

## [0.2.0](https://github.com/zen-rs/skyzen/compare/skyzen-test-v0.1.0...skyzen-test-v0.2.0) - 2026-08-27

### Added

- *(services)* portable SQL migrations, embedded and applied on every backend
- *(services)* [**breaking**] widen object storage options and give SQL an atomic batch
- *(test)* [**breaking**] carry all seven services through TestContext and open up the verbs
- *(services)* stream, range and presign objects instead of buffering them
- *(services)* [**breaking**] let queues delay a send and consume by pull
- *(services)* [**breaking**] give Kv the atomic primitives and paginate its listing
- *(services)* [**breaking**] give service errors an HttpError bridge and a source chain
- *(test)* TTL, cursor pagination, stored metadata, and failure injection in mocks
- *(test)* production error rendering, multi-value headers, and form bodies in TestClient

### Fixed

- *(azure)* give the test SAS credential its new expires_at field
- *(ci)* repair lint, feature-matrix and workerd jobs
- *(test)* resolve numeric object keys in JSON paths; drop redundant helper and tautological tests

### Other

- correct four API shapes the guides had wrong
- make every documented claim match the shipped code
- drop unused async from await-free trait impls
- *(test)* encode form bodies with the same crate the framework decodes them with
- [**breaking**] render and log endpoint errors through one shared pair of helpers

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-test-v0.1.0) - 2026-07-10

### Added

- add OpenAPI support with new macros and documentation generation

### Other

- Improve test helpers and error handling
- Commit remaining Cloudflare and SQL workspace changes
- Complete Form extractor, skyzen::test, and OpenAPI alignment
- Add native Durable Object simulator
- Refactor database API around sqlx-style Db
- Implement portable serverless services and CLI workflow
- Add Durable Object services and Cloudflare glue
- Add per-crate README, docs, and examples
- Fix CI failures across wasm, features, audit, and deps
- Refactor CLI and Cloudflare provider, add helpers
- Add Cloudflare DB support and datasource sugar
- Add services abstraction layer with platform implementations
- Refactor routing and error handling in the Skyzen framework
