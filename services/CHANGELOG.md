# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
