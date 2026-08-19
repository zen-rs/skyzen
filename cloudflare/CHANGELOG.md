# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/zen-rs/skyzen/compare/skyzen-cloudflare-v0.1.0...skyzen-cloudflare-v0.1.1) - 2026-08-19

### Fixed

- *(cloudflare)* drop unnecessary mut on durable router binding

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-cloudflare-v0.1.0) - 2026-07-10

### Added

- *(cloudflare)* export durable objects and add send-safe fetch

### Other

- Centralize peer addr and improve error handling
- Add Cloudflare helpers and bump WASM deps
- Commit remaining Cloudflare and SQL workspace changes
- Add Cloudflare cache and send-safe futures
- Expose wasm env in durable object requests
- Refactor database API around sqlx-style Db
- Implement portable serverless services and CLI workflow
- Add Durable Object services and Cloudflare glue
- Add per-crate README, docs, and examples
- Fix CI failures across wasm, features, audit, and deps
- Refactor CLI and Cloudflare provider, add helpers
- Improve Azure blob listing, CLI args and headers
- Add Cloudflare DB support and datasource sugar
- Add services abstraction layer with platform implementations
