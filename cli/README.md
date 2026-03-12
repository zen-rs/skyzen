# skyzen-cli

[![crates.io](https://img.shields.io/crates/v/skyzen-cli.svg)](https://crates.io/crates/skyzen-cli)
[![License](https://img.shields.io/crates/l/skyzen-cli.svg)](../LICENSE)

Project scaffolding, local development, and deployment CLI for Skyzen apps.

## Overview

The `skyzen` CLI provides a single interface for creating projects, running native local development, and deploying Skyzen applications. For Cloudflare Workers it reads `Skyzen.toml`, generates `.skyzen/gen/wrangler.toml`, and delegates to Wrangler directly. AWS and Azure integration are currently deployment-tooling hooks, not runtime-parity features.

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
skyzen dev --provider aws
skyzen dev --provider azure
```

Native mode is supervised by Skyzen and automatically restarts on source changes. Cloudflare mode uses Wrangler directly; Skyzen does not simulate the Cloudflare environment itself.

### `skyzen deploy`

Deploy the application to the target platform:

```sh
skyzen deploy --provider cloudflare
skyzen deploy --provider aws
skyzen deploy --provider azure
```

## Options

| Flag | Short | Description |
|------|-------|-------------|
| `--provider <name>` | `-p` | Target platform: `native`, `cloudflare`, `aws`, `azure` |
| `--template <name>` | `-t` | Project template for `skyzen new`: `api`, `serverless-events`, `durable-realtime` |
| `--manifest <path>` | `-m` | Path to `Skyzen.toml` (default: `Skyzen.toml` in current directory) |
| `--force` | `-f` | Reuse an existing target directory when scaffolding |
| `--dry-run` | | Preview generated config without writing files |
| `--help` | `-h` | Print usage information |

## Provider Mapping

| Provider | `dev` | `deploy` | Generated Config |
|----------|-------|----------|-----------------|
| Native | `cargo run` with Skyzen watch/restart | — | none |
| Cloudflare | `wrangler dev` | `wrangler deploy` | `.skyzen/gen/wrangler.toml` |
| AWS | `sam local start-api` | `sam deploy` | uses `template` from config |
| Azure | `func start` | `func azure functionapp publish` | uses `project` from config |

The AWS/Azure rows above describe CLI orchestration only. Skyzen's finished runtime surface is native + Cloudflare.

## Skyzen.toml

See the [Skyzen.toml Reference](../docs/skyzen-toml-reference.md) for the full configuration format. Users edit `Skyzen.toml`; generated provider files such as `.skyzen/gen/wrangler.toml` are derived artifacts and are overwritten automatically.

Example:

```toml
[cloudflare]
name = "my-worker"
main = "dist/worker.js"
compatibility_date = "2025-02-01"
workers_dev = true

[aws]
template = "template.yaml"
stack_name = "my-stack"
region = "us-east-1"

[azure]
project = "."
app_name = "my-function-app"
port = 7071
```

## License

MIT or Apache-2.0, at your option.
