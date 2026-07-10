# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-cli-v0.1.0) - 2026-07-10

### Added

- *(cloudflare)* export durable objects and add send-safe fetch

### Fixed

- *(cli)* build scaffold test patch section with toml_edit
- *(cli)* scope unix-only imports to avoid unused-import error on Windows
- *(ci)* correct typo and make scaffold test fetch deps

### Other

- Centralize peer addr and improve error handling
- Add Cloudflare helpers and bump WASM deps
- Prune Providers and Expand HTTP Support
- Improve framework test coverage
- Commit remaining Cloudflare and SQL workspace changes
- Add formal Cloudflare worker build pipeline
- Implement portable serverless services and CLI workflow
- Add per-crate README, docs, and examples
- Support Cloudflare Durable Objects migrations
- Switch native runtime to smol and update deps
- Refactor CLI and Cloudflare provider, add helpers
- Improve Azure blob listing, CLI args and headers
- Add CLI for local emulation and deployment
