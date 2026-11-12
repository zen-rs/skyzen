# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0](https://github.com/zen-rs/skyzen/compare/azure/v0.5.0...azure/v0.6.0) - 2026-11-12

### Added

- *(runtime)* [**breaking**] serve on http-kit 0.8 and zenwave 0.12 with operator io_uring settings
- *(core)* [**breaking**] build Upgraded from a connection's halves, spliceable on Linux

### Fixed

- *(aws)* carry the SDK's timeouts on each request

### Other

- *(deps)* take zenwave and executor-core from git, drop the patch section
- *(runtime)* register the receive ring for incremental consumption

## [0.5.0](https://github.com/zen-rs/skyzen/compare/azure/v0.4.1...azure/v0.5.0) - 2026-11-11

### Added

- *(azure)* pool a client's connections on the thread that uses it
- [**breaking**] adapt to http-kit's frame-based Body
- *(middleware)* RateLimit — fixed-window counter over portable KV ([#169](https://github.com/zen-rs/skyzen/pull/169))

### Fixed

- *(azure)* take an error status from zenwave 0.10 as the response it is
- *(azure)* [**breaking**] run blob, Service Bus, Storage queue and SQL without a tokio runtime

### Other

- move zenwave to 0.10.1
- depend on http-kit 0.5 and zenwave 0.9
- *(azure)* dogfood zenwave for outbound HTTP ([#159](https://github.com/zen-rs/skyzen/pull/159))

## [0.4.1](https://github.com/zen-rs/skyzen/compare/skyzen-azure-v0.4.0...skyzen-azure-v0.4.1) - 2026-11-05

### Other

- updated the following local packages: skyzen-services

## [0.4.0](https://github.com/zen-rs/skyzen/compare/skyzen-azure-v0.3.0...skyzen-azure-v0.4.0) - 2026-10-27

### Added

- [**breaking**] adopt sqlx 0.9 and stop forcing sqlx backends on leaf crates ([#99](https://github.com/zen-rs/skyzen/pull/99))
- *(cli)* add deploy --upload-only for wrangler versions upload ([#98](https://github.com/zen-rs/skyzen/pull/98))

## [0.3.0](https://github.com/zen-rs/skyzen/compare/skyzen-azure-v0.2.0...skyzen-azure-v0.3.0) - 2026-09-05

### Fixed

- *(azure)* [**breaking**] speak the Service Bus REST API directly, dropping legacy azure_core

## [0.2.0](https://github.com/zen-rs/skyzen/compare/skyzen-azure-v0.1.0...skyzen-azure-v0.2.0) - 2026-08-27

### Added

- [**breaking**] reach Azure SQL through a portable T-SQL DbBackend
- *(azure)* read a Storage queue's signed URL from a named variable
- *(runtime)* [**breaking**] drive `#[skyzen::queue]` natively, not only on Cloudflare
- *(azure)* [**breaking**] page blob listings server-side, and own the account credentials
- *(azure)* [**breaking**] bind CosmosKv to a container that already exists
- *(azure)* [**breaking**] give Service Bus a consume side and add Azure Storage queues
- *(services)* [**breaking**] widen object storage options and give SQL an atomic batch
- *(services)* [**breaking**] give Kv the atomic primitives and paginate its listing
- *(services)* [**breaking**] give service errors an HttpError bridge and a source chain

### Fixed

- *(azure)* take the platform TLS path for tiberius
- *(azure)* give the test SAS credential its new expires_at field
- *(runtime,azure)* survive fresh dependency resolves and workerd placeholder cf
- *(azure)* [**breaking**] refuse a Service Bus receive that carries no wait
- *(azure)* release a streamed upload's staged blocks when it gives up
- *(azure)* settle by sequence number, read 410 as a missing queue, block streamed uploads
- *(azure)* drop an unverifiable peek-lock cap and stop coupling `get` to an ETag
- *(ci)* repair lint, feature-matrix and workerd jobs
- *(azure)* blob pagination and metadata, cosmos injection and TTL, bus format

### Other

- state where Azure SQL and a named RDS cluster are declared
- make every documented claim match the shipped code
- *(services)* own the queue envelope codec in one place

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-azure-v0.1.0) - 2026-07-10

### Fixed

- Replace the vulnerable Azure Blob XML dependency chain and update Cosmos to the current SDK API

### Other

- Implement portable serverless services and CLI workflow
- Add per-crate README, docs, and examples
- Switch native runtime to smol and update deps
- Refactor CLI and Cloudflare provider, add helpers
- Improve Azure blob listing, CLI args and headers
- Add Cloudflare DB support and datasource sugar
- Add services abstraction layer with platform implementations
