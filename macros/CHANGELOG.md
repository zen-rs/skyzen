# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/zen-rs/skyzen/compare/macros-v0.2.0...macros-v0.2.1) - 2026-08-29

### Other

- updated the following local packages: skyzen-manifest

## [0.2.0](https://github.com/zen-rs/skyzen/compare/macros-v0.1.2...macros-v0.2.0) - 2026-08-27

### Added

- *(manifest)* let an rds-data wiring name the cluster it addresses
- *(manifest)* wire Azure SQL from a [native.database] declaration
- *(macros)* wire every cloud backend from [native.*] declarations
- *(services)* portable SQL migrations, embedded and applied on every backend
- *(deploy)* serve AWS Lambda and Azure Functions from the same binary
- *(runtime)* [**breaking**] drive `#[skyzen::queue]` natively, not only on Cloudflare
- *(cli)* [**breaking**] rebuild the command surface, the wrangler renderer and the dev loop
- *(manifest)* [**breaking**] one typed Skyzen.toml schema for the CLI and the macros
- *(aws)* [**breaking**] give SQS a consume side and FIFO support, DynamoDB atomics
- *(macros)* [**breaking**] export the email and tail Worker handlers
- *(test)* [**breaking**] carry all seven services through TestContext and open up the verbs
- *(macros)* [**breaking**] generate a named extractor per service, not just per database
- *(extract)* add a typed Path<T> extractor and give path parameters real OpenAPI types
- *(runtime)* [**breaking**] drain connections on Ctrl+C, install hyper's timer, add on_shutdown
- *(routing)* [**breaking**] router layers, fallbacks and build-time wiring validation
- *(macros)* [**breaking**] emit `source()` from #[skyzen::error] and reject inert enum `message`

### Fixed

- *(macros)* say what a mismatched backend does provide
- *(macros)* compare the embedded path in its escaped literal form for Windows
- *(macros)* thread wasm env explicitly, cache endpoint, fix cfg leak

### Other

- *(macros)* fix a typo in the Cosmos DB wiring comment
- make every documented claim match the shipped code
- *(ci)* exercise the execution context on the real workerd runtime
- *(macros)* document the #[skyzen::error] attributes, including #[source]
- Merge pull request #10 from zen-rs/main
- Merge branch 'worktree-agent-a58e0537380c43c31' into claude/skyzen-production-readiness-8kc2dg

## [0.1.2](https://github.com/zen-rs/skyzen/compare/macros-v0.1.1...macros-v0.1.2) - 2026-08-19

### Added

- *(macros)* interpolate fields in #[skyzen::error] messages

## [0.1.1](https://github.com/zen-rs/skyzen/compare/macros-v0.1.0...macros-v0.1.1) - 2026-07-10

### Other

- Merge branch 'main' into dev
- Centralize peer addr and improve error handling
- Add Cloudflare helpers and bump WASM deps
- Improve test helpers and error handling
- fix lints
- Add skyzen-cloudflare-admin crate with reusable control-plane primitives
- Improve framework test coverage
- Complete Form extractor, skyzen::test, and OpenAPI alignment
- Expose wasm env in durable object requests
- Refactor database API around sqlx-style Db
- Implement portable serverless services and CLI workflow
- Add per-crate README, docs, and examples
- Switch native runtime to smol and update deps
- Refactor CLI and Cloudflare provider, add helpers
- Add Cloudflare DB support and datasource sugar

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/macros-v0.1.0) - 2025-12-09

### Added

- enhance logging initialization options and improve handler trait definition
- enhance logging initialization with color-eyre and tracing, add examples for native and worker modes
- enhance project metadata, improve dependency management, and refine HTTP server implementation

### Fixed

- update dependencies and improve WebSocket handling in native module
- remove broken trybuild tests and ignore skyzen-core doctests

### Other

- Fix typos
- update README to enhance clarity and structure, add examples for routing and WebSocket support
- update feature flags for WebSocket support and add procedural macros for Skyzen framework
- remove AGENTS.md and add CLAUDE.md for updated project guidelines
