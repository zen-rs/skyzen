# skyzen-cli

[![crates.io](https://img.shields.io/crates/v/skyzen-cli.svg)](https://crates.io/crates/skyzen-cli)
[![License](https://img.shields.io/crates/l/skyzen-cli.svg)](../LICENSE)

Project scaffolding, local development, provisioning and deployment CLI for Skyzen apps.

## Overview

The `skyzen` CLI is a single interface for creating projects, running them locally, creating the
cloud resources they declare, and deploying them. For Cloudflare Workers it reads `Skyzen.toml`,
builds the wasm target, generates the Worker artifacts, writes `.skyzen/gen/wrangler.toml`, and
then delegates to Wrangler.

The wasm-bindgen generator and the binaryen optimizer are linked into the binary, so
`cargo install skyzen-cli` is the whole toolchain install — there is no `wasm-bindgen` or
`wasm-opt` to install separately, and no version of either to keep in step by hand.

AWS and Azure have service implementations (`skyzen-aws`, `skyzen-azure`) that a native binary can
use, but no deployment adapter yet: `--provider` accepts `native` and `cloudflare`.

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

Create a project from a built-in template:

```sh
skyzen new my-app                              # the `api` template
skyzen new my-app --template minimal
skyzen new jobs-app --template serverless-events
skyzen new room-app --template durable-realtime
skyzen new .                                   # scaffold into the current directory
```

| Template | What it demonstrates |
|----------|----------------------|
| `api` | A portable `[[service]]` KV wired for native *and* Cloudflare, with a handler that uses the generated extractor |
| `minimal` | Two routes and nothing else |
| `serverless-events` | A queue consumer and a cron trigger |
| `durable-realtime` | A WebSocket-serving Durable Object |

The generated `Cargo.toml` carries no Skyzen version numbers: `skyzen new` runs `cargo add` in the
new project, so the versions are whatever the registry actually has. The one pinned dependency is
`wasm-bindgen`, which must match the generator this binary embeds.

`skyzen new` also writes a `.env.example` derived from the template's own manifest.

### `skyzen add`

Add the crates a capability needs, through `cargo add`:

```sh
skyzen add kv redis
skyzen add cloudflare
skyzen add --list postgres     # print the invocations without running them
```

`dev` and `deploy` never edit your `Cargo.toml`. When the manifest declares a capability whose
crate is missing, they fail and name the exact command.

### `skyzen doctor`

Check the toolchain *and* the manifest:

```sh
skyzen doctor
skyzen doctor --provider cloudflare
```

### `skyzen dev`

```sh
skyzen dev                         # native: cargo run, restarted on change
skyzen dev --provider cloudflare   # wasm build + wrangler dev, rebuilt on change
```

Native mode restarts the binary on debounced source changes, after loading `.env` / `.env.local`
into it. Cloudflare mode re-runs the wasm build while `wrangler dev` keeps running, so local state
and the bound port survive an edit.

Trailing arguments reach the runner:

```sh
skyzen dev --provider cloudflare -- --test-scheduled
skyzen dev -- --port 3000
```

### `skyzen migrate`

Apply SQL migrations from `migrations/` to every declared D1 database:

```sh
skyzen migrate --local
skyzen migrate --env staging
```

### `skyzen build`

Produce the artifacts without running or deploying:

```sh
skyzen build --provider cloudflare --release
```

Every build prints the final `.wasm` size, raw and gzipped; release builds run through `wasm-opt -Os`.

### `skyzen provision`

Create the KV namespaces, R2 buckets, D1 databases and queues the manifest declares but has no id
for, and write the ids back into `Skyzen.toml`:

```sh
skyzen provision --provider cloudflare --dry-run
skyzen provision --provider cloudflare
```

Idempotent: anything that already has an id is skipped.

### `skyzen deploy`

```sh
skyzen deploy --provider cloudflare
skyzen deploy --provider cloudflare --env staging
skyzen deploy --provider cloudflare --dry-run   # real build, `wrangler deploy --dry-run`
```

### `skyzen logs` / `skyzen secret`

```sh
skyzen logs
skyzen logs -- --format json     # forwarded to `wrangler tail`
skyzen secret set API_KEY        # value read from stdin
skyzen secret list
```

### `skyzen completions`

```sh
skyzen completions zsh > "${fpath[1]}/_skyzen"
```

## Options

| Flag | Short | Description |
|------|-------|-------------|
| `--provider <name>` | `-p` | Target platform: `native`, `cloudflare` |
| `--manifest <path>` | `-m` | Path to `Skyzen.toml` (default: `Skyzen.toml` in the current directory) |
| `--env <name>` | `-e` | Select a `[cloudflare.env.<name>]` overlay and forward it to wrangler |
| `--dry-run` | | Print what would happen instead of doing it |
| `--template <name>` | `-t` | `skyzen new`: `api`, `minimal`, `serverless-events`, `durable-realtime` |
| `--force` | `-f` | `skyzen new`: reuse a non-empty directory, **keeping** files that already exist |
| `--overwrite` | | `skyzen new`: reuse a non-empty directory, **replacing** files that already exist |

Global flags are accepted before or after the subcommand.

## Provider Mapping

| Provider | `dev` | `deploy` | Generated Config |
|----------|-------|----------|-----------------|
| Native | `cargo run` with Skyzen watch/restart | — | none |
| Cloudflare | wasm build + `wrangler dev --local`, rebuilt on change | `wrangler deploy` | `.skyzen/gen/wrangler.toml`, `dist/worker.js`, `dist/worker_bg.js`, `dist/worker_bg.wasm` |

## Skyzen.toml

See the [Skyzen.toml Reference](../docs/skyzen-toml-reference.md) for the full configuration
format. Users edit `Skyzen.toml`; generated provider files such as `.skyzen/gen/wrangler.toml` and
the Cloudflare Worker artifacts under `dist/` are derived and are overwritten automatically.

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
