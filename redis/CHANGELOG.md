# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/zen-rs/skyzen/compare/skyzen-redis-v0.2.0...skyzen-redis-v0.2.1) - 2026-09-05

### Other

- updated the following local packages: skyzen-services

## [0.2.0](https://github.com/zen-rs/skyzen/compare/skyzen-redis-v0.1.0...skyzen-redis-v0.2.0) - 2026-08-27

### Added

- *(redis)* implement the atomic KV primitives and a from_env constructor
- *(services)* [**breaking**] give Kv the atomic primitives and paginate its listing
- *(services)* [**breaking**] give service errors an HttpError bridge and a source chain

### Fixed

- *(ci)* repair lint, feature-matrix and workerd jobs
- *(redis)* make runtime features exclusive, escape scan globs, add TTL

### Other

- make every documented claim match the shipped code

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-redis-v0.1.0) - 2026-07-10

### Other

- Add per-crate README, docs, and examples
- Fix CI failures across wasm, features, audit, and deps
- Improve Azure blob listing, CLI args and headers
- Add services abstraction layer with platform implementations
