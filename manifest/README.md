# skyzen-manifest

The typed model of `Skyzen.toml`, shared by the [Skyzen](https://github.com/zen-rs/skyzen)
procedural macros and the `skyzen` CLI.

`Skyzen.toml` declares portable capabilities (`[[service]]`, `[[database]]`) once and wires them
per target (`[native.*]`, `[cloudflare.*]`). Two very different consumers read it:

- `#[skyzen::main]` reads it at compile time and generates the backend construction and the
  typed extractors.
- `skyzen dev` / `skyzen deploy` read it to render `wrangler.toml` and drive the build.

They must never disagree about what a section means, so both deserialize through the schema in
this crate rather than through parsers of their own. Every struct carries
`#[serde(deny_unknown_fields)]`, and discriminants such as `type` and `backend` are enums, so a
typo or an unsupported value fails at parse time instead of being silently dropped.

```rust
use skyzen_manifest::Manifest;

let manifest = Manifest::load(std::path::Path::new("Skyzen.toml"))?;
let cloudflare = manifest.cloudflare(Some("staging"))?;
# Ok::<_, Box<dyn std::error::Error>>(())
```

See the [`Skyzen.toml` reference](https://github.com/zen-rs/skyzen/blob/main/docs/skyzen-toml-reference.md)
for the full key list.

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
