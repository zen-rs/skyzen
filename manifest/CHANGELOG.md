# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/zen-rs/skyzen/compare/skyzen-manifest-v0.1.0...skyzen-manifest-v0.1.1) - 2026-08-29

### Added

- *(cli)* refuse committed secrets on CLI load
- *(manifest)* interpolate deploy-time environment placeholders

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-manifest-v0.1.0) - 2026-08-27

### Added

- *(manifest)* let an rds-data wiring name the cluster it addresses
- *(manifest)* wire Azure SQL from a [native.database] declaration
- *(manifest)* model native wiring as per-backend tagged variants
- *(services)* portable SQL migrations, embedded and applied on every backend
- *(deploy)* serve AWS Lambda and Azure Functions from the same binary
- *(runtime)* [**breaking**] drive `#[skyzen::queue]` natively, not only on Cloudflare
- *(cli)* [**breaking**] rebuild the command surface, the wrangler renderer and the dev loop
- *(manifest)* [**breaking**] one typed Skyzen.toml schema for the CLI and the macros

### Fixed

- *(ci)* satisfy machete and the Windows dead-code gate
- *(azure)* give the test SAS credential its new expires_at field

### Other

- make every documented claim match the shipped code
