# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/zen-rs/skyzen/compare/skyzen-cloudflare-admin-v0.1.0...skyzen-cloudflare-admin-v0.1.1) - 2026-08-19

### Other

- correct README extractor list and OpenAPI gating
- rewrite README

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-cloudflare-admin-v0.1.0) - 2026-07-10

### Added

- enhance logging initialization options and improve handler trait definition
- enhance logging initialization with color-eyre and tracing, add examples for native and worker modes
- enhance project metadata, improve dependency management, and refine HTTP server implementation

### Fixed

- *(deps)* clear RustSec advisories and drop unused dependency
- update dependencies and improve WebSocket handling in native module

### Other

- Improve test helpers and error handling
- Prune Providers and Expand HTTP Support
- Add skyzen-cloudflare-admin crate with reusable control-plane primitives
- Complete Form extractor, skyzen::test, and OpenAPI alignment
- Add durable SQL development guide
- Refactor database API around sqlx-style Db
- Implement portable serverless services and CLI workflow
- Add per-crate README, docs, and examples
- Support Cloudflare Durable Objects migrations
- Add CLI for local emulation and deployment
- Add Cloudflare DB support and datasource sugar
- update README to enhance clarity and structure, add examples for routing and WebSocket support
- remove AGENTS.md and add CLAUDE.md for updated project guidelines
