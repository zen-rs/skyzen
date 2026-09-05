# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/zen-rs/skyzen/compare/skyzen-s3-v0.2.0...skyzen-s3-v0.2.1) - 2026-09-05

### Other

- updated the following local packages: skyzen-services

## [0.2.0](https://github.com/zen-rs/skyzen/compare/skyzen-s3-v0.1.0...skyzen-s3-v0.2.0) - 2026-08-27

### Added

- *(s3)* stream, range, presign and multipart, and wire every put option
- *(services)* [**breaking**] widen object storage options and give SQL an atomic batch
- *(services)* [**breaking**] give service errors an HttpError bridge and a source chain

### Fixed

- *(azure)* give the test SAS credential its new expires_at field
- *(s3)* surface full AWS error context and precise not-found detection

### Other

- make every documented claim match the shipped code

## [0.1.0](https://github.com/zen-rs/skyzen/releases/tag/skyzen-s3-v0.1.0) - 2026-07-10

### Fixed

- *(deps)* clear RustSec advisories and drop unused dependency

### Other

- Add per-crate README, docs, and examples
- Add services abstraction layer with platform implementations
