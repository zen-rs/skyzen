# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/zen-rs/skyzen/compare/core-v0.1.1...core-v0.2.0) - 2026-08-27

### Added

- *(deploy)* serve AWS Lambda and Azure Functions from the same binary
- *(sse,diagnostics)* keep SSE connections alive and explain unsatisfied trait bounds
- *(extract)* add a typed Path<T> extractor and give path parameters real OpenAPI types
- *(core)* [**breaking**] enforce the request body limit and refuse a second body read
- *(routing)* [**breaking**] router layers, fallbacks and build-time wiring validation
- *(core)* [**breaking**] own the middleware trait, request-body limit and wiring requirements
- *(core)* [**breaking**] preserve HTTP status through `?` and make `Error` a real error type

### Fixed

- *(ci)* clear the two checks a current toolchain already fails
- *(core)* fall back to the status number when it has no canonical reason
- *(core)* restore no_std build and export a coherent error triple

### Other

- make every documented claim match the shipped code
- drop unused async from await-free trait impls
- [**breaking**] render and log endpoint errors through one shared pair of helpers
- cover error_response redaction and add hyper end-to-end tests
- *(core)* rewrite Extractor and Responder trait documentation

## [0.1.1](https://github.com/zen-rs/skyzen/compare/core-v0.1.0...core-v0.1.1) - 2026-07-10

### Other

- Merge branch 'main' into dev
- Centralize peer addr and improve error handling
- Add per-crate README, docs, and examples

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/core-v0.1.0) - 2025-12-09

### Added

- enhance Extractor trait to require 'static bound for improved safety and consistency
- enhance logging initialization options and improve handler trait definition
- enhance logging initialization with color-eyre and tracing, add examples for native and worker modes
- enhance dependency configurations, improve extractor and responder traits, and add logging initialization
- Update wasm-bindgen CLI version, enhance worker configuration, and improve request/response handling
- enhance project metadata, improve dependency management, and refine HTTP server implementation
- update dependencies and improve HTTP response handling

### Fixed

- update dependencies, improve executor handling, and add hyper example
- update dependencies and improve WebSocket handling in native module
- reorganize http_kit imports for better clarity and structure
- remove broken trybuild tests and ignore skyzen-core doctests
- update HttpError status methods to return Option<StatusCode>

### Other

- Fix typos
- update CI and release workflows, add test workflow, and modify dependencies
- update code examples in error module to use skyzen_core::StatusCode
- fix code block formatting in error module examples
- update README to enhance clarity and structure, add examples for routing and WebSocket support
- update dependencies and improve error handling
- remove AGENTS.md and add CLAUDE.md for updated project guidelines
- Refactor OpenAPI schema handling and remove unused code
- Refactor routing and error handling in the Skyzen framework
- Refactor error handling and OpenAPI schema generation
- Reconstruct workspace & add lints
