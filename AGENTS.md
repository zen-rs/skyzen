# Repository Guidelines

## Project Structure & Module Organization
Skyzen is a Cargo workspace (`Cargo.toml` at the root) that exposes the main framework in `src/` while housing shared traits in `core/` and the optional Hyper backend in `hyper/`. Look to `src/routing`, `src/middleware`, and `src/responder` for the primary HTTP abstractions, and to `src/test_helper.rs` for reusable test scaffolding. Workspace-level configuration, features, and lint rules live in the root `Cargo.toml`, and build artifacts are emitted to `target/` (never edit files there directly).

## Build, Test, and Development Commands
Use standard Cargo workflows from the repository root:
```
cargo fmt
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
```
`cargo fmt` should run before every commit, `cargo clippy` enforces the workspace lint list (including pedantic and nursery groups), and `cargo test` exercises every crate, feature, and async path. For backend-specific work, `cargo test -p skyzen-core` or `cargo test -p skyzen-hyper` focus on the respective crate. Enable verbose diagnostics with `RUST_LOG=debug` when debugging integration tests.

## Coding Style & Naming Conventions
Follow Rust 2021 conventions: modules and files in `snake_case`, types and traits in `PascalCase`, functions in `snake_case`, and feature flags mirroring the declarations in `[features]`. Keep public APIs documented to satisfy the workspace `missing_docs` lint, derive `Debug` where practical to satisfy `missing_debug_implementations`, and prefer small focused modules that mirror the folder layout (e.g., `routing::node`, `middleware::stack`). Run `cargo fmt` and `cargo clippy` with no warnings before requesting review.

## Testing Guidelines
Tests live alongside implementation modules or inside each crate’s `tests` directory; integration helpers (e.g., `src/test_helper.rs`) should be reused rather than duplicated. Name tests with the behavior under test (`handles_route_conflicts`) and use `#[tokio::test(flavor = "multi_thread")]` when exercising async handlers. New features must include positive, negative, and concurrency coverage when applicable, and regression tests should reference the issue or scenario they prevent in a doc comment.

## Commit & Pull Request Guidelines
Recent history favors imperative verbs and optional scopes (`feat: enhance project metadata`, `Refactor skyzen service…`). Keep commits focused on one concern, mention the touched crate when useful, and include “feat/fix/chore” prefixes for user-facing changes. Pull requests should link related issues, summarize architectural impact, list new commands or feature flags, and paste the results of `cargo fmt`, `cargo clippy`, and `cargo test`. Include screenshots or cURL transcripts when demonstrating handler behavior so reviewers can reproduce locally.

## Architecture & Extension Tips
`skyzen-core` defines traits (`RequestHandler`, middleware traits) that the top-level crate re-exports, while `skyzen-hyper` provides the Hyper-powered executor. Prefer adding new abstractions in `core/` so both the native and Hyper backends stay aligned, and hide backend-specific glue behind feature flags declared in the workspace manifest. When expanding middleware, wire new extractors or responders through `src/utils` to keep handler signatures minimal.
