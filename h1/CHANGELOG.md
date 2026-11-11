# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/zen-rs/skyzen/compare/h1/v0.0.0...h1/v0.1.0) - 2026-11-11

### Added

- *(h1)* advertise an alternative service on every response
- *(runtime)* terminate TLS with http-kit-tls over owned-buffer streams
- *(runtime)* [**breaking**] serve unix sockets through http-kit-io, hyper included
- [**breaking**] #[skyzen::main] serves one compio runtime per core over skyzen-h1
- *(h1)* serve any acceptor, with an optional connection limit
- [**breaking**] Server over http-kit-io listeners with per-thread !Send routers
- request body decompression middleware
- *(durable)* [**breaking**] require every DurableObject to declare PERSIST
- [**breaking**] keep a Durable Object's struct alive for the instance
- *(cli)* add `skyzen openapi`, and title documents by registration
- *(openapi)* [**breaking**] serve the same document on every target
- *(openapi)* [**breaking**] serve Scalar as the default API docs UI
- *(sql)* [**breaking**] decode rows by the type they are fetched into
- *(services)* portable SQL migrations, embedded and applied on every backend
- *(deploy)* serve AWS Lambda and Azure Functions from the same binary
- *(macros)* [**breaking**] export the email and tail Worker handlers
- *(macros)* [**breaking**] emit `source()` from #[skyzen::error] and reject inert enum `message`
- enhance logging initialization options and improve handler trait definition
- enhance logging initialization with color-eyre and tracing, add examples for native and worker modes
- enhance project metadata, improve dependency management, and refine HTTP server implementation

### Fixed

- *(openapi)* [**breaking**] require a schema where a schema is the contract
- *(core)* fall back to the status number when it has no canonical reason
- *(examples,docs)* drop the `map_err` tax now that service errors carry a status
- update dependencies and improve WebSocket handling in native module

### Other

- Merge pull request #41 from zen-rs/feat/unified-secrets
- describe the unified secret system
- *(readme)* add GitHub Secrets and .env integration
- Revert "docs(readme): add GitHub Secrets and .env integration"
- *(readme)* add GitHub Secrets and .env integration
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
- correct README extractor list and OpenAPI gating
- rewrite README
- Prune Providers and Expand HTTP Support
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
