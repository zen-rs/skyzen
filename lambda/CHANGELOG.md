# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.1](https://github.com/zen-rs/skyzen/compare/lambda/v0.3.0...lambda/v0.3.1) - 2026-11-12

### Other

- update Cargo.toml dependencies

## [0.3.0](https://github.com/zen-rs/skyzen/compare/lambda/v0.2.1...lambda/v0.3.0) - 2026-11-11

### Added

- [**breaking**] Server over http-kit-io listeners with per-thread !Send routers
- [**breaking**] bound on SendEndpoint where an endpoint's future crosses threads
- *(middleware)* RequestId, Trace and MatchedPath ([#162](https://github.com/zen-rs/skyzen/pull/162))

### Fixed

- *(extract)* [**breaking**] trust proxy headers only from configured proxies ([#160](https://github.com/zen-rs/skyzen/pull/160))

## [0.2.1](https://github.com/zen-rs/skyzen/compare/lambda-v0.2.0...lambda-v0.2.1) - 2026-11-05

### Other

- updated the following local packages: skyzen-services

## [0.2.0](https://github.com/zen-rs/skyzen/compare/lambda-v0.1.1...lambda-v0.2.0) - 2026-10-27

### Added

- [**breaking**] adopt sqlx 0.9 and stop forcing sqlx backends on leaf crates ([#99](https://github.com/zen-rs/skyzen/pull/99))

## [0.1.1](https://github.com/zen-rs/skyzen/compare/lambda-v0.1.0...lambda-v0.1.1) - 2026-09-05

### Other

- updated the following local packages: skyzen-core, skyzen-services

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/lambda-v0.1.0) - 2026-08-27

### Added

- *(deploy)* serve AWS Lambda and Azure Functions from the same binary

### Fixed

- *(ci)* clear the two checks a current toolchain already fails
- *(azure)* give the test SAS credential its new expires_at field
