# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/zen-rs/skyzen/compare/skyzen-cloudflare-admin-v0.2.0...skyzen-cloudflare-admin-v0.3.0) - 2026-09-05

### Added

- *(cli)* add `skyzen openapi`, and title documents by registration
- *(openapi)* [**breaking**] serve the same document on every target
- *(openapi)* [**breaking**] serve Scalar as the default API docs UI

### Other

- Merge pull request #41 from zen-rs/feat/unified-secrets
- *(readme)* add GitHub Secrets and .env integration

## [0.2.0](https://github.com/zen-rs/skyzen/compare/skyzen-cloudflare-admin-v0.1.1...skyzen-cloudflare-admin-v0.2.0) - 2026-08-27

### Added

- *(services)* portable SQL migrations, embedded and applied on every backend
- *(deploy)* serve AWS Lambda and Azure Functions from the same binary
- *(macros)* [**breaking**] export the email and tail Worker handlers
- *(macros)* [**breaking**] emit `source()` from #[skyzen::error] and reject inert enum `message`

### Fixed

- *(core)* fall back to the status number when it has no canonical reason
- *(examples,docs)* drop the `map_err` tax now that service errors carry a status
- *(cloudflare-admin)* honest crate description, pruned errors, into_result tests

### Other

- state where Azure SQL and a named RDS cluster are declared
- document every backend a manifest can now declare
- correct four API shapes the guides had wrong
- make every documented claim match the shipped code
- the native queue cell contradicted the manifest that wires SQS natively
- *(ci)* exercise the execution context on the real workerd runtime
- cover the new extractors end to end and make the docs match what ships
- *(middleware)* state that the body limit is advertised, not yet enforced
- describe the middleware trait, layers, fallbacks and wiring validation
- Rewrite README and AGENTS.md
- Merge pull request #10 from zen-rs/main
- Merge branch 'worktree-agent-a1962c2303a9353eb' into claude/skyzen-production-readiness-8kc2dg

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
