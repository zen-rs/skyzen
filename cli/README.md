# skyzen-cli

[![crates.io](https://img.shields.io/crates/v/skyzen-cli.svg)](https://crates.io/crates/skyzen-cli)
[![License](https://img.shields.io/crates/l/skyzen-cli.svg)](../LICENSE)

Unified local emulation and deployment CLI for Skyzen apps.

## Overview

The `skyzen` CLI provides a single interface for running and deploying Skyzen applications across Cloudflare Workers, AWS Lambda, and Azure Functions. It reads `Skyzen.toml` to generate provider-specific configuration and delegates to the provider's native tooling.

## Installation

```sh
cargo install skyzen-cli
```

Or run from the Skyzen repository:

```sh
cargo run -p skyzen-cli -- <command>
```

## Commands

### `skyzen doctor`

Check that required provider tools are installed:

```sh
skyzen doctor
```

### `skyzen dev`

Start a local development server using the provider's emulator:

```sh
skyzen dev --provider cloudflare
skyzen dev --provider aws
skyzen dev --provider azure
```

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
| `--provider <name>` | `-p` | Target platform: `cloudflare`, `aws`, `azure` |
| `--manifest <path>` | `-m` | Path to `Skyzen.toml` (default: `Skyzen.toml` in current directory) |
| `--dry-run` | | Preview generated config without writing files |
| `--help` | `-h` | Print usage information |

## Provider Mapping

| Provider | `dev` | `deploy` | Generated Config |
|----------|-------|----------|-----------------|
| Cloudflare | `wrangler dev` | `wrangler deploy` | `.skyzen/gen/wrangler.toml` |
| AWS | `sam local start-api` | `sam deploy` | uses `template` from config |
| Azure | `func start` | `func azure functionapp publish` | uses `project` from config |

## Skyzen.toml

See the [Skyzen.toml Reference](../docs/skyzen-toml-reference.md) for the full configuration format.

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
