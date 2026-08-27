# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
