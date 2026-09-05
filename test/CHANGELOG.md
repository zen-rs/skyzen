# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
