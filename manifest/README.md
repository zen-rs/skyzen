# skyzen-manifest

The typed model of `Skyzen.toml`, shared by the [Skyzen](https://github.com/zen-rs/skyzen)
procedural macros and the `skyzen` CLI.

`Skyzen.toml` declares portable capabilities (`[[service]]`, `[[database]]`) once and wires them
per target (`[native.*]`, `[cloudflare.*]`, `[aws]`, `[azure]`). Two very different consumers read
it:

- `#[skyzen::main]` reads it at compile time and generates the backend construction, the typed
  extractors, and the Azure queue triggers the runtime mounts.
- `skyzen dev` / `skyzen deploy` read it to render provider configuration (`wrangler.toml`, the
  Azure Functions bundle, the `cargo lambda` flags) and drive the build.

They must never disagree about what a section means, so both deserialize through the schema in
this crate rather than through parsers of their own. Every struct carries
`#[serde(deny_unknown_fields)]`, and discriminants such as `type` and `backend` are enums, so a
typo or an unsupported value fails at parse time instead of being silently dropped.

The same argument covers migrations, so the `migrations` module lives here too: it is the one
reader of a `<version>_<name>.sql` directory, used by `skyzen::embed_migrations!` at compile time
and by `skyzen migrate` at deploy time. If those two disagreed about which files count or how a
checksum is computed, a deployment would look clean while running different SQL from the binary.

```rust
use skyzen_manifest::Manifest;

let manifest = Manifest::load(std::path::Path::new("Skyzen.toml"))?;
let cloudflare = manifest.cloudflare(Some("staging"))?;
# Ok::<_, Box<dyn std::error::Error>>(())
```

String values may contain `${NAME}` placeholders. The CLI expands them from the process
environment (GitHub Actions secrets, `.env`) through `Manifest::load_with`. `Manifest::load`
leaves them as written, which is what `#[skyzen::main]` uses so a missing CI secret cannot fail
`cargo build`.

See the [`Skyzen.toml` reference](https://github.com/zen-rs/skyzen/blob/main/docs/skyzen-toml-reference.md)
for the full key list.

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
