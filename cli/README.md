# skyzen-cli

[![crates.io](https://img.shields.io/crates/v/skyzen-cli.svg)](https://crates.io/crates/skyzen-cli)
[![License](https://img.shields.io/crates/l/skyzen-cli.svg)](../LICENSE)

Project scaffolding, local development, and deployment CLI for Skyzen apps.

## Overview

The `skyzen` CLI provides a single interface for creating projects, running native local development, and deploying Skyzen applications. For Cloudflare Workers it reads `Skyzen.toml`, builds the wasm target, generates Worker artifacts, writes `.skyzen/gen/wrangler.toml`, and then delegates to Wrangler. AWS and Azure integration are currently deployment-tooling hooks, not runtime-parity features.

## Installation

```sh
cargo install skyzen-cli
```

Or run from the Skyzen repository:

```sh
cargo run -p skyzen-cli -- <command>
```

## Commands

### `skyzen new`

Create a new project from a built-in template:

```sh
skyzen new my-app --template api
skyzen new jobs-app --template serverless-events
skyzen new room-app --template durable-realtime
```

### `skyzen doctor`

Check that required provider tools are installed:

```sh
skyzen doctor
```

### `skyzen dev`

Start local development:

```sh
skyzen dev                         # native watch + restart
skyzen dev --provider cloudflare
```

Native mode is supervised by Skyzen and automatically restarts on source changes. Cloudflare mode uses Wrangler directly after Skyzen prepares the Worker artifacts.

### `skyzen deploy`

Deploy the application to the target platform:

```sh
skyzen deploy --provider cloudflare
```

## Options

| Flag | Short | Description |
|------|-------|-------------|
| `--provider <name>` | `-p` | Target platform: `native`, `cloudflare` |
| `--template <name>` | `-t` | Project template for `skyzen new`: `api`, `serverless-events`, `durable-realtime` |
| `--manifest <path>` | `-m` | Path to `Skyzen.toml` (default: `Skyzen.toml` in current directory) |
| `--force` | `-f` | Reuse an existing target directory when scaffolding |
| `--dry-run` | | Preview generated config without writing files |
| `--help` | `-h` | Print usage information |

## Provider Mapping

| Provider | `dev` | `deploy` | Generated Config |
|----------|-------|----------|-----------------|
| Native | `cargo run` with Skyzen watch/restart | — | none |
| Cloudflare | `wrangler dev` | `wrangler deploy` | `.skyzen/gen/wrangler.toml`, `dist/worker.js`, `dist/worker_bg.js`, `dist/worker_bg.wasm` |

## Skyzen.toml

See the [Skyzen.toml Reference](../docs/skyzen-toml-reference.md) for the full configuration format. Users edit `Skyzen.toml`; generated provider files such as `.skyzen/gen/wrangler.toml` and Cloudflare Worker artifacts under `dist/` are derived artifacts and are overwritten automatically.

Example:

```toml
[cloudflare]
name = "my-worker"
main = "dist/worker.js"
compatibility_date = "2025-02-01"
workers_dev = true
```

## License

MIT or Apache-2.0, at your option.
