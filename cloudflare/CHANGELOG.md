# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/zen-rs/skyzen/compare/skyzen-cloudflare-v0.2.0...skyzen-cloudflare-v0.2.1) - 2026-08-29

### Other

- updated the following local packages: skyzen

## [0.2.0](https://github.com/zen-rs/skyzen/compare/skyzen-cloudflare-v0.1.1...skyzen-cloudflare-v0.2.0) - 2026-08-27

### Added

- *(ws)* [**breaking**] give websocket sessions an error channel and make `.ws` work on wasm32
- *(cloudflare)* close the outbound cf, static assets and service-binding props gaps
- *(macros)* [**breaking**] export the email and tail Worker handlers
- *(durable)* [**breaking**] block concurrency, target jurisdictions, and let an object opt out of blob state
- *(cloudflare)* [**breaking**] reach the depth of KV, R2, Queues, D1 and service bindings
- *(services)* [**breaking**] widen object storage options and give SQL an atomic batch
- *(services)* [**breaking**] bind rich SQL types, bound single-row fetches, share one query builder
- *(services)* [**breaking**] give Kv the atomic primitives and paginate its listing
- *(runtime)* [**breaking**] drain connections on Ctrl+C, install hyper's timer, add on_shutdown
- *(services)* [**breaking**] give service errors an HttpError bridge and a source chain

### Fixed

- *(docs)* correct three spellings the typos job flags
- *(cloudflare)* durable persistence, SQL integer safety, D1 and header fixes
- *(cloudflare)* repair queue payload encoding, KV pagination/TTL, R2 metadata

### Other

- correct four API shapes the guides had wrong
- make every documented claim match the shipped code
- drop unused async from await-free trait impls
- [**breaking**] render and log endpoint errors through one shared pair of helpers
- Merge pull request #10 from zen-rs/main
- *(cloudflare)* dedupe JS-handle boilerplate and make the wasm build warning-free

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
