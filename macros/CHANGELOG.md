# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
