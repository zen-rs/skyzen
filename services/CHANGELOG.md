# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/zen-rs/skyzen/compare/skyzen-services-v0.2.0...skyzen-services-v0.2.1) - 2026-09-05

### Other

- updated the following local packages: skyzen-core

## [0.2.0](https://github.com/zen-rs/skyzen/compare/skyzen-services-v0.1.0...skyzen-services-v0.2.0) - 2026-08-27

### Added

- [**breaking**] reach Azure SQL through a portable T-SQL DbBackend
- *(services)* portable SQL migrations, embedded and applied on every backend
- *(services)* [**breaking**] report which batch messages a queue send rejected
- *(services)* [**breaking**] widen object storage options and give SQL an atomic batch
- *(test)* [**breaking**] carry all seven services through TestContext and open up the verbs
- *(services)* [**breaking**] bind rich SQL types, bound single-row fetches, share one query builder
- *(services)* stream, range and presign objects instead of buffering them
- *(services)* [**breaking**] let queues delay a send and consume by pull
- *(services)* [**breaking**] give Kv the atomic primitives and paginate its listing
- *(routing)* [**breaking**] router layers, fallbacks and build-time wiring validation
- *(services)* [**breaking**] give service errors an HttpError bridge and a source chain
- *(services)* add put_with options to ObjectStorage
- *(services)* add TTL support and richer error variants to KeyValueStore

### Fixed

- *(azure)* give the test SAS credential its new expires_at field
- *(docs)* correct three spellings the typos job flags
- *(ci)* repair lint, feature-matrix and workerd jobs
- *(services)* robust SQL row decoding, O(n) placeholder rewriting, real driver features

### Other

- correct four API shapes the guides had wrong
- make every documented claim match the shipped code
- *(services)* own the queue envelope codec in one place
- drop unused async from await-free trait impls
- *(services)* [**breaking**] delete MaybeSend and generate the object-safe service layer
- describe the middleware trait, layers, fallbacks and wiring validation
- drop stale CfDurableSqlite references after its removal
- Merge branch 'worktree-agent-a12991cc13373d8e3' into claude/skyzen-production-readiness-8kc2dg

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-services-v0.1.0) - 2026-07-10

### Other

- Centralize peer addr and improve error handling
- Improve test helpers and error handling
- fix lints
- Add skyzen-cloudflare-admin crate with reusable control-plane primitives
- Improve framework test coverage
- Commit remaining Cloudflare and SQL workspace changes
- Require send futures in service wrappers
- Add native Durable Object simulator
- Gate sql helpers to native builds
- Refactor database API around sqlx-style Db
- Implement portable serverless services and CLI workflow
- Add Durable Object services and Cloudflare glue
- Add per-crate README, docs, and examples
- Add Cloudflare DB support and datasource sugar
- Add services abstraction layer with platform implementations
