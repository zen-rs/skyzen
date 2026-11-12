# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0](https://github.com/zen-rs/skyzen/compare/aws/v0.4.0...aws/v0.5.0) - 2026-11-12

### Added

- *(runtime)* [**breaking**] serve on http-kit 0.8 and zenwave 0.12 with operator io_uring settings
- *(core)* [**breaking**] build Upgraded from a connection's halves, spliceable on Linux

### Fixed

- *(aws)* carry the SDK's timeouts on each request

### Other

- *(deps)* take zenwave and executor-core from git, drop the patch section
- *(runtime)* register the receive ring for incremental consumption

## [0.4.0](https://github.com/zen-rs/skyzen/compare/aws/v0.3.1...aws/v0.4.0) - 2026-11-11

### Added

- *(aws)* pool an SDK client's connections on the thread that uses it
- *(aws)* restore the credential_process credentials source without tokio
- *(middleware)* RateLimit — fixed-window counter over portable KV ([#169](https://github.com/zen-rs/skyzen/pull/169))

### Fixed

- *(aws)* [**breaking**] fetch credentials on the coordinating thread's pools
- *(aws)* report a transport that cannot be built as a connector error
- *(aws)* hand the SDK's connect and read timeouts to zenwave
- *(aws)* honor the SDK's connect and read timeouts on the zenwave client
- *(aws)* [**breaking**] do the AWS SDK's I/O on zenwave's native backend
- satisfy current clippy across the workspace

## [0.3.1](https://github.com/zen-rs/skyzen/compare/skyzen-aws-v0.3.0...skyzen-aws-v0.3.1) - 2026-11-05

### Other

- updated the following local packages: skyzen-services, skyzen-s3

## [0.3.0](https://github.com/zen-rs/skyzen/compare/skyzen-aws-v0.2.1...skyzen-aws-v0.3.0) - 2026-10-27

### Added

- [**breaking**] adopt sqlx 0.9 and stop forcing sqlx backends on leaf crates ([#99](https://github.com/zen-rs/skyzen/pull/99))

## [0.2.1](https://github.com/zen-rs/skyzen/compare/skyzen-aws-v0.2.0...skyzen-aws-v0.2.1) - 2026-09-05

### Other

- updated the following local packages: skyzen-services, skyzen-s3

## [0.2.0](https://github.com/zen-rs/skyzen/compare/skyzen-aws-v0.1.0...skyzen-aws-v0.2.0) - 2026-08-27

### Added

- *(aws)* build an RdsDataDb from named parts
- *(services)* portable SQL migrations, embedded and applied on every backend
- *(aws)* add an RDS Data API DbBackend for Aurora
- *(aws)* [**breaking**] give SQS a consume side and FIFO support, DynamoDB atomics
- *(services)* [**breaking**] give Kv the atomic primitives and paginate its listing
- *(services)* [**breaking**] give service errors an HttpError bridge and a source chain

### Fixed

- *(docs)* correct three spellings the typos job flags
- *(ci)* repair lint, feature-matrix and workerd jobs
- *(aws)* DynamoDB TTL and schema errors, SQS wire format and batch detail

### Other

- state where Azure SQL and a named RDS cluster are declared
- make every documented claim match the shipped code
- assert on emptiness in the form clippy asks for
- *(aws)* say what a whole-chunk SQS batch failure loses

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-aws-v0.1.0) - 2026-07-10

### Fixed

- *(deps)* clear RustSec advisories and drop unused dependency

### Other

- Add per-crate README, docs, and examples
- Fix CI failures across wasm, features, audit, and deps
- Add Cloudflare DB support and datasource sugar
- Add services abstraction layer with platform implementations
