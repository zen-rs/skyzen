# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/zen-rs/skyzen/compare/skyzen-cli-v0.2.1...skyzen-cli-v0.3.0) - 2026-09-05

### Added

- *(cli)* [**breaking**] deliver runtime variables to Azure app settings
- *(cli)* deliver runtime variables to Lambda through the SDK
- *(cli)* deliver [[secret]] values to Cloudflare over stdin
- *(manifest)* [**breaking**] make secrets a portable capability with `[[secret]]`

### Fixed

- *(cli)* pin the scaffolded compatibility_date instead of reading the clock

### Other

- Merge pull request #50 from zen-rs/feat/skyzen-openapi-command
- Merge pull request #48 from zen-rs/feat/openapi-on-wasm
- *(cli)* join the wrangler config path the way the plan does
- describe the unified secret system
- *(cli)* plans are ordered steps with typed stdin
- *(cli)* resolve runtime variables through one Environment

## [0.2.1](https://github.com/zen-rs/skyzen/compare/skyzen-cli-v0.2.0...skyzen-cli-v0.2.1) - 2026-08-29

### Added

- *(cli)* refuse committed secrets on CLI load
- *(manifest)* interpolate deploy-time environment placeholders

## [0.2.0](https://github.com/zen-rs/skyzen/compare/skyzen-cli-v0.1.0...skyzen-cli-v0.2.0) - 2026-08-27

### Added

- *(manifest)* let an rds-data wiring name the cluster it addresses
- *(manifest)* wire Azure SQL from a [native.database] declaration
- *(cli)* install, preflight and report every declarative backend
- *(cli)* give the api template a database and a migration to apply
- *(cli)* report D1 migration status instead of refusing to
- *(services)* portable SQL migrations, embedded and applied on every backend
- *(deploy)* serve AWS Lambda and Azure Functions from the same binary
- *(cli)* add d1 migrations and forward runner arguments from dev
- *(cli)* [**breaking**] rebuild the command surface, the wrangler renderer and the dev loop
- *(manifest)* [**breaking**] one typed Skyzen.toml schema for the CLI and the macros

### Fixed

- *(cli)* assert the bundle location by path components, not a slashed string
- *(cli)* match bundle paths by component so the Azure tests pass on Windows
- *(ci)* satisfy machete and the Windows dead-code gate
- *(cli)* keep Skyzen.toml optional, and stop gating a build on runtime variables
- *(cli)* keep --dry-run read-only and stop leaking test temp dirs
- *(cli)* wire queue/scheduled worker exports and repair broken templates

### Other

- make every documented claim match the shipped code
- assert on emptiness in the form clippy asks for

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
