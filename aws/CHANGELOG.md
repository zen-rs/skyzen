# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
