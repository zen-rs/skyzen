//! Turning a Rust package into the files wrangler uploads.
//!
//! `cargo build --target wasm32-unknown-unknown` → in-process wasm-bindgen → `wasm-opt` →
//! a generated ESM shim. Every step is linked into this binary rather than shelled out to, so
//! `cargo install skyzen-cli` is the whole toolchain install.

use crate::{
    output,
    project::{Project, WASM_TARGET},
};
use anyhow::{Context, Result};
use askama::Template;
use flate2::{write::GzEncoder, Compression};
use skyzen_manifest::CloudflareSection;
use std::{
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use wasm_bindgen_cli_support::Bindgen;

/// One member of the Worker's default export, beyond `fetch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventMember {
    /// The member's name, which is also the wasm export it forwards to.
    pub name: &'static str,
    /// The member's parameter list, forwarded verbatim.
    pub args: &'static str,
    /// The Rust attribute that produces the matching wasm export.
    pub macro_hint: &'static str,
}

/// Deliver a batch from a queue the manifest declares a consumer for.
const QUEUE: EventMember = EventMember {
    name: "queue",
    args: "batch, env, ctx",
    macro_hint: "#[skyzen::queue]",
};
/// Fire on one of the manifest's cron triggers.
const SCHEDULED: EventMember = EventMember {
    name: "scheduled",
    args: "event, env, ctx",
    macro_hint: "#[skyzen::scheduled]",
};
/// Receive a routed email.
const EMAIL: EventMember = EventMember {
    name: "email",
    args: "message, env, ctx",
    macro_hint: "#[skyzen::email]",
};
/// Receive another Worker's tail events.
const TAIL: EventMember = EventMember {
    name: "tail",
    args: "events, env, ctx",
    macro_hint: "#[skyzen::tail]",
};

/// The event members the manifest asks the shim to export.
///
/// A queue handler is implied by a consumer and a scheduled handler by a cron trigger. Email
/// routing and tail consumers are configured on the sending side, so those two are opted into
/// explicitly with `[cloudflare.handlers]`.
pub fn event_members(config: &CloudflareSection) -> Vec<EventMember> {
    let mut members = Vec::new();
    if !config.queues.consumers.is_empty() {
        members.push(QUEUE);
    }
    if !config.triggers.crons.is_empty() {
        members.push(SCHEDULED);
    }
    if config.handlers.email {
        members.push(EMAIL);
    }
    if config.handlers.tail {
        members.push(TAIL);
    }
    members
}

/// A Durable Object class the wasm module must export, and the name the shim publishes it under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DurableObjectExport {
    /// The name wrangler binds to, which is the Rust struct's name.
    pub public_name: String,
    /// The name `#[skyzen::durable_object]` exports from the wasm module.
    pub bindings_export_name: String,
}

/// Everything needed to produce the Worker artifacts.
#[derive(Debug, Clone)]
pub struct BuildPlan {
    /// The project root, which cargo is run from.
    pub root_dir: PathBuf,
    /// The package's `Cargo.toml`.
    pub cargo_manifest_path: PathBuf,
    /// Where cargo leaves the `cdylib`.
    pub wasm_artifact_path: PathBuf,
    /// The directory the generated files land in.
    pub output_dir: PathBuf,
    /// The generated ESM entry point, which `wrangler.toml`'s `main` points at.
    pub entry_js_path: PathBuf,
    /// The wasm-bindgen JS glue.
    pub bindings_js_path: PathBuf,
    /// The wasm module the glue loads.
    pub wasm_output_path: PathBuf,
    /// The stem wasm-bindgen names its output after.
    pub bindgen_out_name: String,
    /// Durable Object classes to re-export from the shim.
    pub durable_exports: Vec<DurableObjectExport>,
    /// Extra members of the Worker's default export.
    pub event_members: Vec<EventMember>,
    /// Build the wasm artifact with `--release`.
    pub release: bool,
    /// Run `wasm-opt` over the generated module.
    pub optimize: bool,
}

impl BuildPlan {
    /// A one-line description for progress output and `--dry-run`.
    pub fn describe(&self) -> String {
        format!(
            "build Cloudflare worker artifacts into {}",
            self.output_dir.display()
        )
    }
}

/// Build the Worker artifacts.
///
/// # Errors
///
/// Fails when cargo fails, when the expected wasm artifact is absent, when wasm-bindgen or
/// wasm-opt fail, or when the manifest names a Durable Object class the Rust code does not define.
pub fn run(plan: &BuildPlan) -> Result<()> {
    run_cargo_build(plan)?;
    generate_wasm_bindings(plan)?;
    if plan.optimize {
        optimize_wasm(&plan.wasm_output_path)?;
    }
    report_size(&plan.wasm_output_path)?;
    Ok(())
}

/// The wasm-bindgen version this binary embeds.
///
/// `wasm_bindgen_shared` is a direct dependency pinned to the same exact version
/// `wasm-bindgen-cli-support` requires, so cargo unifies them and this is, by construction, the
/// generator's version. `version()` appends a build hash when one was compiled in, which is not
/// part of the version an application would pin.
pub fn embedded_wasm_bindgen_version() -> String {
    wasm_bindgen_shared::version()
        .split(' ')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Check the application's `wasm-bindgen` agrees with the embedded generator.
///
/// wasm-bindgen requires the macro that produced a wasm module and the generator that consumes it
/// to match exactly; when they do not, the failure surfaces as an opaque schema-version error deep
/// inside the build. Checking beforehand turns that into one sentence and one command.
///
/// # Errors
///
/// Fails when the versions differ, naming the `cargo update` that fixes it.
pub fn check_wasm_bindgen_agreement(project: &Project) -> Result<()> {
    let embedded = embedded_wasm_bindgen_version();
    let Some(resolved) = project.resolved_dependency_version("wasm-bindgen", WASM_TARGET)? else {
        // Nothing in the wasm graph uses wasm-bindgen, so there is no macro output to disagree
        // with. The build will fail later for a clearer reason (no exported `fetch`).
        return Ok(());
    };

    if resolved == embedded {
        return Ok(());
    }

    anyhow::bail!(
        "wasm-bindgen version mismatch: {} resolves wasm-bindgen {resolved}, but skyzen embeds the \
         {embedded} bindings generator. wasm-bindgen requires an exact match between the macro that \
         produced the module and the generator that consumes it.\nRun:\n  cargo update -p wasm-bindgen --precise {embedded}",
        project.manifest_path().display()
    )
}

fn run_cargo_build(plan: &BuildPlan) -> Result<()> {
    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--target")
        .arg(WASM_TARGET)
        .arg("--lib")
        .arg("--manifest-path")
        .arg(&plan.cargo_manifest_path);
    if plan.release {
        command.arg("--release");
    }
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .current_dir(&plan.root_dir)
        .status()
        .context("failed to launch cargo build for the Cloudflare worker")?;

    if !status.success() {
        anyhow::bail!(
            "cargo build failed while preparing Cloudflare worker artifacts from {}",
            plan.cargo_manifest_path.display()
        );
    }

    if !plan.wasm_artifact_path.exists() {
        anyhow::bail!(
            "expected wasm artifact at {} after cargo build, but it was not produced",
            plan.wasm_artifact_path.display()
        );
    }

    Ok(())
}

fn generate_wasm_bindings(plan: &BuildPlan) -> Result<()> {
    fs::create_dir_all(&plan.output_dir)
        .with_context(|| format!("failed to create {}", plan.output_dir.display()))?;

    let generated_entry_path = plan
        .output_dir
        .join(format!("{}.js", plan.bindgen_out_name));
    for path in [
        &generated_entry_path,
        &plan.bindings_js_path,
        &plan.entry_js_path,
        &plan.wasm_output_path,
    ] {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("failed to remove stale artifact {}", path.display()))?;
        }
    }

    let mut bindgen = Bindgen::new();
    bindgen
        .input_path(&plan.wasm_artifact_path)
        .out_name(&plan.bindgen_out_name)
        .web(true)
        .context("failed to configure wasm-bindgen output mode")?
        .typescript(false);
    bindgen.generate(&plan.output_dir).with_context(|| {
        format!(
            "failed to generate wasm bindings from {} into {}",
            plan.wasm_artifact_path.display(),
            plan.output_dir.display()
        )
    })?;

    fs::rename(&generated_entry_path, &plan.bindings_js_path).with_context(|| {
        format!(
            "failed to rename generated bindings {} -> {}",
            generated_entry_path.display(),
            plan.bindings_js_path.display()
        )
    })?;

    let bindings_source = fs::read_to_string(&plan.bindings_js_path)
        .with_context(|| format!("failed to read {}", plan.bindings_js_path.display()))?;
    verify_durable_exports(&bindings_source, &plan.durable_exports)?;

    let bindings_name = file_name(&plan.bindings_js_path)?;
    let wasm_name = file_name(&plan.wasm_output_path)?;
    let shim = render_worker_shim(
        bindings_name,
        wasm_name,
        &plan.durable_exports,
        &plan.event_members,
    )?;
    fs::write(&plan.entry_js_path, shim)
        .with_context(|| format!("failed to write {}", plan.entry_js_path.display()))?;

    Ok(())
}

/// Fail when the manifest binds a Durable Object class the wasm module does not export.
///
/// Without this the mistake surfaces as a `class ... not found` error the first time the object is
/// instantiated in production, which is one of the hardest failures to trace back to a manifest
/// typo.
fn verify_durable_exports(bindings_source: &str, exports: &[DurableObjectExport]) -> Result<()> {
    let missing: Vec<&DurableObjectExport> = exports
        .iter()
        .filter(|export| !exports_symbol(bindings_source, &export.bindings_export_name))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let details = missing
        .iter()
        .map(|export| {
            format!(
                "  class_name = \"{}\" needs `#[skyzen::durable_object] pub struct {}` (exported as `{}`)",
                export.public_name, export.public_name, export.bindings_export_name
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::bail!(
        "Skyzen.toml declares Durable Object bindings the wasm module does not export:\n{details}"
    )
}

/// Whether the generated bindings export `symbol` as a whole identifier.
fn exports_symbol(source: &str, symbol: &str) -> bool {
    source.match_indices(symbol).any(|(index, matched)| {
        let before_is_word = source[..index]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_alphanumeric() || character == '_');
        let after_is_word = source[index + matched.len()..]
            .chars()
            .next()
            .is_some_and(|character| character.is_alphanumeric() || character == '_');
        !before_is_word && !after_is_word
    })
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(OsStr::to_str)
        .with_context(|| format!("path has no usable file name: {}", path.display()))
}

/// Render the ESM shim that wrangler loads.
///
/// # Errors
///
/// Fails only when the template itself cannot render.
pub fn render_worker_shim(
    bindings_name: &str,
    wasm_name: &str,
    durable_exports: &[DurableObjectExport],
    event_members: &[EventMember],
) -> Result<String> {
    WorkerShimTemplate {
        bindings_name,
        wasm_name,
        durable_exports,
        event_members,
    }
    .render()
    .context("failed to render the Cloudflare worker shim")
}

#[derive(Template)]
#[template(path = "worker.mjs.tmpl", escape = "none")]
struct WorkerShimTemplate<'a> {
    bindings_name: &'a str,
    wasm_name: &'a str,
    durable_exports: &'a [DurableObjectExport],
    event_members: &'a [EventMember],
}

/// Shrink the generated module with binaryen.
///
/// The optimizer is linked in rather than shelled out to, so it inherits the same zero-install
/// property as the bindings generator: there is no `wasm-opt` for the user to install and no
/// version of it to keep in step.
fn optimize_wasm(wasm_path: &Path) -> Result<()> {
    let before = file_size(wasm_path)?;
    let optimized_path = wasm_path.with_extension("opt.wasm");

    // `all_features` matches the baseline rustc emits for wasm32-unknown-unknown, which has moved
    // well past the MVP; without it binaryen rejects the module it is handed.
    wasm_opt::OptimizationOptions::new_optimize_for_size()
        .all_features()
        .debug_info(false)
        .run(wasm_path, &optimized_path)
        .with_context(|| format!("wasm-opt failed on {}", wasm_path.display()))?;

    fs::rename(&optimized_path, wasm_path).with_context(|| {
        format!(
            "failed to replace {} with the optimized module",
            wasm_path.display()
        )
    })?;

    let after = file_size(wasm_path)?;
    output::step(format!(
        "wasm-opt -Os: {} -> {}",
        human_bytes(before),
        human_bytes(after)
    ));
    Ok(())
}

/// Print the artifact's raw and compressed sizes.
///
/// Cloudflare enforces a *compressed* size limit, so the gzipped figure is the one that decides
/// whether a deploy is accepted; printing it every build makes the trend visible before wrangler
/// rejects the upload.
fn report_size(wasm_path: &Path) -> Result<()> {
    let bytes =
        fs::read(wasm_path).with_context(|| format!("failed to read {}", wasm_path.display()))?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&bytes)
        .context("failed to compress the generated wasm module")?;
    let compressed = encoder
        .finish()
        .context("failed to compress the generated wasm module")?;

    output::step(format!(
        "{}: {} raw, {} gzipped",
        wasm_path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("worker wasm"),
        human_bytes(bytes.len() as u64),
        human_bytes(compressed.len() as u64)
    ));
    Ok(())
}

fn file_size(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len())
}

fn human_bytes(bytes: u64) -> String {
    /// Thresholds, largest first, so the first match is the unit to use.
    const UNITS: &[(u64, &str)] = &[(1 << 20, "MiB"), (1 << 10, "KiB")];
    for &(scale, unit) in UNITS {
        if bytes >= scale {
            #[allow(clippy::cast_precision_loss)]
            let value = bytes as f64 / scale as f64;
            return format!("{value:.2} {unit}");
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::{
        embedded_wasm_bindgen_version, event_members, exports_symbol, human_bytes,
        render_worker_shim, verify_durable_exports, DurableObjectExport, EMAIL, QUEUE, SCHEDULED,
        TAIL,
    };
    use skyzen_manifest::Manifest;

    fn cloudflare(source: &str) -> skyzen_manifest::CloudflareSection {
        Manifest::parse(source, "Skyzen.toml", ".")
            .expect("valid manifest")
            .cloudflare(None)
            .expect("base")
            .expect("cloudflare section")
            .clone()
    }

    fn shim(durable: &[DurableObjectExport], members: &[super::EventMember]) -> String {
        render_worker_shim("worker_bg.js", "worker_bg.wasm", durable, members).expect("render")
    }

    #[test]
    fn the_embedded_generator_version_is_a_bare_semver() {
        let version = embedded_wasm_bindgen_version();
        assert!(!version.contains(' '), "{version}");
        assert_eq!(version.split('.').count(), 3, "{version}");
    }

    #[test]
    fn the_shim_reexports_local_durable_objects() {
        let rendered = shim(
            &[
                DurableObjectExport {
                    public_name: "Scheduler".to_owned(),
                    bindings_export_name: "SchedulerObject".to_owned(),
                },
                DurableObjectExport {
                    public_name: "Room".to_owned(),
                    bindings_export_name: "RoomObject".to_owned(),
                },
            ],
            &[],
        );

        assert!(rendered.contains("import init, * as wasmExports from \"./worker_bg.js\";"));
        assert!(rendered.contains("import wasmUrl from \"./worker_bg.wasm\";"));
        assert!(
            rendered.contains("export { SchedulerObject as Scheduler } from \"./worker_bg.js\";")
        );
        assert!(rendered.contains("export { RoomObject as Room } from \"./worker_bg.js\";"));
        // Durable Object classes must be usable in a fresh isolate before the first fetch event,
        // so the shim initializes wasm at module load.
        assert!(rendered.contains("await ensureInitialized();\n\nexport default {"));
    }

    #[test]
    fn the_shim_omits_what_the_manifest_does_not_declare() {
        let rendered = shim(&[], &[]);
        assert!(!rendered.contains(" as Scheduler }"));
        assert!(rendered.contains("export default {"));
        assert!(!rendered.contains("async queue("));
        assert!(!rendered.contains("async scheduled("));
    }

    #[test]
    fn every_event_member_forwards_and_fails_loudly_when_the_export_is_missing() {
        let rendered = shim(&[], &[QUEUE, SCHEDULED, EMAIL, TAIL]);
        for (name, args, hint) in [
            ("queue", "batch, env, ctx", "#[skyzen::queue]"),
            ("scheduled", "event, env, ctx", "#[skyzen::scheduled]"),
            ("email", "message, env, ctx", "#[skyzen::email]"),
            ("tail", "events, env, ctx", "#[skyzen::tail]"),
        ] {
            assert!(
                rendered.contains(&format!("async {name}({args})")),
                "{rendered}"
            );
            assert!(
                rendered.contains(&format!("return wasmExports.{name}({args});")),
                "{rendered}"
            );
            assert!(rendered.contains(hint), "{rendered}");
        }
    }

    #[test]
    fn queue_and_cron_members_are_inferred_while_email_and_tail_are_opted_into() {
        let inferred = cloudflare(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.triggers]\ncrons = [\"* * * * *\"]\n\n\
             [[cloudflare.queues.consumers]]\nqueue = \"jobs\"\n",
        );
        let names: Vec<_> = event_members(&inferred)
            .into_iter()
            .map(|member| member.name)
            .collect();
        assert_eq!(names, ["queue", "scheduled"]);

        let opted_in = cloudflare(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.handlers]\nemail = true\ntail = true\n",
        );
        let names: Vec<_> = event_members(&opted_in)
            .into_iter()
            .map(|member| member.name)
            .collect();
        assert_eq!(names, ["email", "tail"]);
    }

    #[test]
    fn a_durable_binding_whose_class_the_wasm_lacks_fails_the_build() {
        let exports = [DurableObjectExport {
            public_name: "Room".to_owned(),
            bindings_export_name: "RoomObject".to_owned(),
        }];

        verify_durable_exports("export class RoomObject {}", &exports).expect("present");

        let error =
            verify_durable_exports("export class LobbyObject {}", &exports).expect_err("absent");
        assert!(
            error.to_string().contains("#[skyzen::durable_object]"),
            "{error}"
        );
        assert!(error.to_string().contains("Room"), "{error}");
    }

    #[test]
    fn symbol_matching_respects_identifier_boundaries() {
        assert!(exports_symbol("export class RoomObject {}", "RoomObject"));
        assert!(!exports_symbol(
            "export class BigRoomObject {}",
            "RoomObject"
        ));
        assert!(!exports_symbol(
            "export class RoomObjectV2 {}",
            "RoomObject"
        ));
    }

    #[test]
    fn sizes_are_reported_in_the_unit_a_reader_can_compare() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.00 KiB");
        assert_eq!(human_bytes(3 << 20), "3.00 MiB");
    }
}
