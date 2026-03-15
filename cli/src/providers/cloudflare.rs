use crate::{
    args::Action,
    manifest::{
        CfD1Database, CfDurableBinding, CfDurableMigration, CfDurableRenamedClass, CfKvNamespace,
        CfQueueConsumer, CfQueueProducer, CfR2Bucket, CloudflareSection, LoadedManifest,
    },
    providers::{CommandPlan, GeneratedFile, InternalStep, ProviderPlan, RunMode},
};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use wasm_bindgen_cli_support::Bindgen;

const WORKER_SHIM_TEMPLATE: &str = include_str!("../../assets/cloudflare/worker.mjs");

#[derive(Debug, Clone)]
pub struct CloudflareBuildPlan {
    pub root_dir: PathBuf,
    pub cargo_manifest_path: PathBuf,
    pub wasm_artifact_path: PathBuf,
    pub output_dir: PathBuf,
    pub entry_js_path: PathBuf,
    pub bindings_js_path: PathBuf,
    pub wasm_output_path: PathBuf,
    pub bindgen_out_name: String,
}

pub fn prepare(action: Action, manifest: &LoadedManifest) -> Result<ProviderPlan> {
    let config = manifest
        .data
        .cloudflare
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing [cloudflare] section in Skyzen.toml"))?;
    let build = resolve_build_plan(manifest, config)?;
    warn_missing_portable_bindings(manifest, config);
    let wrangler = render_wrangler(config, &manifest.root_dir, &build.entry_js_path)?;
    let wrangler_path = manifest.root_dir.join(".skyzen/gen/wrangler.toml");

    let command = match action {
        Action::Dev => CommandPlan {
            program: "wrangler".to_owned(),
            args: vec![
                "dev".to_owned(),
                "--local".to_owned(),
                "--config".to_owned(),
                path_string(&wrangler_path)?,
            ],
            cwd: Some(manifest.root_dir.clone()),
        },
        Action::Deploy => CommandPlan {
            program: "wrangler".to_owned(),
            args: vec![
                "deploy".to_owned(),
                "--config".to_owned(),
                path_string(&wrangler_path)?,
            ],
            cwd: Some(manifest.root_dir.clone()),
        },
        Action::Doctor => unreachable!("doctor is handled in providers::prepare"),
        Action::New => unreachable!("new is handled in the CLI entrypoint"),
    };

    Ok(ProviderPlan {
        commands: vec![command],
        generated_files: vec![GeneratedFile {
            path: wrangler_path,
            contents: wrangler,
        }],
        internal_steps: vec![InternalStep::CloudflareBuild(build)],
        run_mode: RunMode::Once,
    })
}

pub fn run_build(plan: &CloudflareBuildPlan) -> Result<()> {
    run_cargo_build(plan)?;
    generate_wasm_bindings(plan)?;
    Ok(())
}

fn warn_missing_portable_bindings(manifest: &LoadedManifest, config: &CloudflareSection) {
    for service in &manifest.data.service {
        let Some(wiring) = config.service.get(&service.name) else {
            eprintln!(
                "[skyzen] warning: missing [cloudflare.service.{}] wiring for portable service `{}`",
                service.name, service.name
            );
            continue;
        };

        let binding = &wiring.binding;
        let exists = match service.service_type.as_str() {
            "kv" => config
                .kv_namespaces
                .iter()
                .any(|entry| entry.binding == *binding),
            "storage" => config
                .r2_buckets
                .iter()
                .any(|entry| entry.binding == *binding),
            "queue" => config
                .queues
                .producers
                .iter()
                .any(|entry| entry.binding == *binding),
            other => {
                eprintln!(
                    "[skyzen] warning: unsupported portable service type `{other}` for `{}`",
                    service.name
                );
                continue;
            }
        };

        if !exists {
            eprintln!(
                "[skyzen] warning: binding `{binding}` for portable service `{}` is not declared in the matching Cloudflare binding section",
                service.name
            );
        }
    }

    for database in &manifest.data.database {
        if database.database_type != "sql" {
            eprintln!(
                "[skyzen] warning: unsupported portable database type `{}` for `{}`",
                database.database_type, database.name
            );
            continue;
        }

        let Some(wiring) = config.database.get(&database.name) else {
            eprintln!(
                "[skyzen] warning: missing [cloudflare.database.{}] wiring for portable database `{}`",
                database.name, database.name
            );
            continue;
        };

        if !config
            .d1_databases
            .iter()
            .any(|entry| entry.binding == wiring.binding)
        {
            eprintln!(
                "[skyzen] warning: binding `{}` for portable database `{}` is not declared in [[cloudflare.d1_databases]]",
                wiring.binding, database.name
            );
        }
    }
}

fn render_wrangler(
    config: &CloudflareSection,
    root_dir: &Path,
    entry_js_path: &Path,
) -> Result<String> {
    let name = config.name.clone().unwrap_or_else(|| {
        root_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("skyzen-app")
            .to_owned()
    });
    let compatibility_date = config
        .compatibility_date
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cloudflare.compatibility_date is required (example: compatibility_date = \"2025-02-01\")"
            )
        })?;
    let wrangler_dir = root_dir.join(".skyzen/gen");
    let main = relative_posix_path(&wrangler_dir, entry_js_path)?;

    let mut out = String::new();
    push_quoted_assignment(&mut out, "name", &name);
    push_quoted_assignment(&mut out, "main", &main);
    push_quoted_assignment(&mut out, "compatibility_date", compatibility_date);

    if !config.compatibility_flags.is_empty() {
        out.push_str("compatibility_flags = [");
        for (index, flag) in config.compatibility_flags.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&quoted(flag));
        }
        out.push_str("]\n");
    }
    if let Some(account_id) = &config.account_id {
        push_quoted_assignment(&mut out, "account_id", account_id);
    }
    if let Some(workers_dev) = config.workers_dev {
        push_assignment(
            &mut out,
            "workers_dev",
            if workers_dev { "true" } else { "false" },
        );
    }
    if let Some(route) = &config.route {
        push_quoted_assignment(&mut out, "route", route);
    }
    if let Some(zone_id) = &config.zone_id {
        push_quoted_assignment(&mut out, "zone_id", zone_id);
    }

    append_vars(&mut out, &config.vars);
    append_triggers(&mut out, &config.triggers.crons);
    append_kv(&mut out, &config.kv_namespaces);
    append_r2(&mut out, &config.r2_buckets);
    append_d1(&mut out, &config.d1_databases);
    append_queue_producers(&mut out, &config.queues.producers);
    append_queue_consumers(&mut out, &config.queues.consumers);
    append_durable_bindings(&mut out, &config.durable_objects.bindings);
    append_durable_migrations(&mut out, &config.durable_objects.migrations);
    Ok(out)
}

fn append_vars(out: &mut String, vars: &std::collections::BTreeMap<String, String>) {
    if vars.is_empty() {
        return;
    }
    out.push_str("\n[vars]\n");
    for (key, value) in vars {
        push_quoted_assignment(out, key, value);
    }
}

fn append_triggers(out: &mut String, crons: &[String]) {
    if crons.is_empty() {
        return;
    }
    out.push_str("\n[triggers]\n");
    push_quoted_array_assignment(out, "crons", crons);
}

fn append_kv(out: &mut String, entries: &[CfKvNamespace]) {
    for entry in entries {
        out.push_str("\n[[kv_namespaces]]\n");
        push_quoted_assignment(out, "binding", &entry.binding);
        push_quoted_assignment(out, "id", &entry.id);
        if let Some(preview_id) = &entry.preview_id {
            push_quoted_assignment(out, "preview_id", preview_id);
        }
    }
}

fn append_r2(out: &mut String, entries: &[CfR2Bucket]) {
    for entry in entries {
        out.push_str("\n[[r2_buckets]]\n");
        push_quoted_assignment(out, "binding", &entry.binding);
        push_quoted_assignment(out, "bucket_name", &entry.bucket_name);
        if let Some(preview_bucket_name) = &entry.preview_bucket_name {
            push_quoted_assignment(out, "preview_bucket_name", preview_bucket_name);
        }
    }
}

fn append_d1(out: &mut String, entries: &[CfD1Database]) {
    for entry in entries {
        out.push_str("\n[[d1_databases]]\n");
        push_quoted_assignment(out, "binding", &entry.binding);
        push_quoted_assignment(out, "database_name", &entry.database_name);
        push_quoted_assignment(out, "database_id", &entry.database_id);
        if let Some(preview_database_id) = &entry.preview_database_id {
            push_quoted_assignment(out, "preview_database_id", preview_database_id);
        }
    }
}

fn append_queue_producers(out: &mut String, entries: &[CfQueueProducer]) {
    for entry in entries {
        out.push_str("\n[[queues.producers]]\n");
        push_quoted_assignment(out, "binding", &entry.binding);
        push_quoted_assignment(out, "queue", &entry.queue);
    }
}

fn append_queue_consumers(out: &mut String, entries: &[CfQueueConsumer]) {
    for entry in entries {
        out.push_str("\n[[queues.consumers]]\n");
        push_quoted_assignment(out, "queue", &entry.queue);
    }
}

fn append_durable_bindings(out: &mut String, entries: &[CfDurableBinding]) {
    for entry in entries {
        out.push_str("\n[[durable_objects.bindings]]\n");
        push_quoted_assignment(out, "name", &entry.name);
        push_quoted_assignment(out, "class_name", &entry.class_name);
        if let Some(script_name) = &entry.script_name {
            push_quoted_assignment(out, "script_name", script_name);
        }
    }
}

fn append_durable_migrations(out: &mut String, entries: &[CfDurableMigration]) {
    for entry in entries {
        out.push_str("\n[[migrations]]\n");
        push_quoted_assignment(out, "tag", &entry.tag);
        push_quoted_array_assignment(out, "new_classes", &entry.new_classes);
        push_quoted_array_assignment(out, "new_sqlite_classes", &entry.new_sqlite_classes);
        push_quoted_array_assignment(out, "deleted_classes", &entry.deleted_classes);
        push_renamed_classes_assignment(out, &entry.renamed_classes);
    }
}

fn push_quoted_array_assignment(out: &mut String, key: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    out.push_str(key);
    out.push_str(" = [");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&quoted(value));
    }
    out.push_str("]\n");
}

fn push_renamed_classes_assignment(out: &mut String, values: &[CfDurableRenamedClass]) {
    if values.is_empty() {
        return;
    }
    out.push_str("renamed_classes = [");
    for (index, item) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str("{ from = ");
        out.push_str(&quoted(&item.from));
        out.push_str(", to = ");
        out.push_str(&quoted(&item.to));
        out.push_str(" }");
    }
    out.push_str("]\n");
}

fn push_assignment(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(value);
    out.push('\n');
}

fn push_quoted_assignment(out: &mut String, key: &str, value: &str) {
    let value = quoted(value);
    push_assignment(out, key, &value);
}

fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn relative_posix_path(from_dir: &Path, to_path: &Path) -> Result<String> {
    let relative = pathdiff::diff_paths(to_path, from_dir).with_context(|| {
        format!(
            "failed to derive relative path from {} to {}",
            from_dir.display(),
            to_path.display()
        )
    })?;
    let value = relative
        .to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", relative.display()))?;
    Ok(value.replace('\\', "/"))
}

fn resolve_build_plan(
    manifest: &LoadedManifest,
    config: &CloudflareSection,
) -> Result<CloudflareBuildPlan> {
    let cargo_manifest_path = manifest.root_dir.join("Cargo.toml");
    if !cargo_manifest_path.exists() {
        anyhow::bail!(
            "missing Cargo.toml at project root {}; Cloudflare build requires a Rust package root",
            manifest.root_dir.display()
        );
    }

    let metadata = load_cargo_metadata(&manifest.root_dir, &cargo_manifest_path)?;
    let package = metadata
        .packages
        .iter()
        .find(|package| package.manifest_path == cargo_manifest_path)
        .with_context(|| {
            format!(
                "failed to find package metadata for {}",
                cargo_manifest_path.display()
            )
        })?;
    let target = package
        .targets
        .iter()
        .find(|target| target.crate_types.iter().any(|kind| kind == "cdylib"))
        .with_context(|| {
            format!(
                "{} must define a [lib] target with crate-type including \"cdylib\" for Cloudflare builds",
                cargo_manifest_path.display()
            )
        })?;

    let entry_rel = parse_entry_path(config)?;
    let output_dir = manifest
        .root_dir
        .join(entry_rel.parent().unwrap_or_else(|| Path::new("")));
    let entry_stem = entry_rel
        .file_stem()
        .and_then(OsStr::to_str)
        .filter(|stem| !stem.is_empty())
        .with_context(|| {
            format!(
                "cloudflare.main must be a JavaScript file path, got {}",
                entry_rel.display()
            )
        })?;

    if entry_stem.ends_with("_bg") {
        anyhow::bail!(
            "cloudflare.main file stem must not end with `_bg`; Skyzen reserves that suffix for generated support artifacts"
        );
    }

    let target_directory = metadata.target_directory;
    let wasm_artifact_path = target_directory
        .join("wasm32-unknown-unknown")
        .join("debug")
        .join(format!("{}.wasm", target.name));
    let entry_js_path = manifest.root_dir.join(&entry_rel);
    let bindings_js_path = output_dir.join(format!("{entry_stem}_bg.js"));
    let wasm_output_path = output_dir.join(format!("{entry_stem}_bg.wasm"));

    Ok(CloudflareBuildPlan {
        root_dir: manifest.root_dir.clone(),
        cargo_manifest_path,
        wasm_artifact_path,
        output_dir,
        entry_js_path,
        bindings_js_path,
        wasm_output_path,
        bindgen_out_name: entry_stem.to_owned(),
    })
}

fn parse_entry_path(config: &CloudflareSection) -> Result<PathBuf> {
    let entry = config.main.as_deref().unwrap_or("dist/worker.js");
    let path = PathBuf::from(entry);
    if path.is_absolute() {
        anyhow::bail!("cloudflare.main must be relative to the project root, got {entry}");
    }
    if path.extension().and_then(OsStr::to_str) != Some("js") {
        anyhow::bail!("cloudflare.main must point to a .js file, got {entry}");
    }
    Ok(path)
}

fn run_cargo_build(plan: &CloudflareBuildPlan) -> Result<()> {
    let status = Command::new("cargo")
        .arg("build")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--lib")
        .arg("--manifest-path")
        .arg(&plan.cargo_manifest_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .current_dir(&plan.root_dir)
        .status()
        .with_context(|| "failed to launch cargo build for Cloudflare worker")?;

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

fn generate_wasm_bindings(plan: &CloudflareBuildPlan) -> Result<()> {
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
    bindgen
        .generate(&plan.output_dir)
        .with_context(|| {
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

    let bindings_name = plan
        .bindings_js_path
        .file_name()
        .and_then(OsStr::to_str)
        .expect("bindings file name");
    let wasm_name = plan
        .wasm_output_path
        .file_name()
        .and_then(OsStr::to_str)
        .expect("wasm file name");
    let shim = WORKER_SHIM_TEMPLATE
        .replace("__SKYZEN_BINDINGS_JS__", bindings_name)
        .replace("__SKYZEN_WASM__", wasm_name);
    fs::write(&plan.entry_js_path, shim)
        .with_context(|| format!("failed to write {}", plan.entry_js_path.display()))?;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackageMetadata>,
    target_directory: PathBuf,
}

#[derive(Debug, Deserialize)]
struct CargoPackageMetadata {
    manifest_path: PathBuf,
    targets: Vec<CargoTargetMetadata>,
}

#[derive(Debug, Deserialize)]
struct CargoTargetMetadata {
    name: String,
    crate_types: Vec<String>,
}

fn load_cargo_metadata(root_dir: &Path, cargo_manifest_path: &Path) -> Result<CargoMetadata> {
    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--no-deps")
        .arg("--manifest-path")
        .arg(cargo_manifest_path)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .current_dir(root_dir)
        .output()
        .with_context(|| "failed to launch cargo metadata for Cloudflare worker build")?;

    if !output.status.success() {
        anyhow::bail!(
            "cargo metadata failed while preparing Cloudflare worker build for {}",
            cargo_manifest_path.display()
        );
    }

    serde_json::from_slice(&output.stdout)
        .with_context(|| "failed to parse cargo metadata JSON for Cloudflare worker build")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{CfDurableObjects, CfQueues, CfTriggers};
    use std::{collections::BTreeMap, fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

    #[test]
    fn renders_wrangler_with_bindings() {
        let mut vars = BTreeMap::new();
        vars.insert("APP_ENV".to_owned(), "dev".to_owned());

        let section = CloudflareSection {
            name: Some("skyzen-worker".to_owned()),
            main: Some("dist/worker.js".to_owned()),
            compatibility_date: Some("2025-02-01".to_owned()),
            compatibility_flags: vec!["nodejs_compat".to_owned()],
            account_id: Some("abc".to_owned()),
            workers_dev: Some(true),
            route: None,
            zone_id: None,
            vars,
            triggers: CfTriggers::default(),
            service: BTreeMap::new(),
            database: BTreeMap::new(),
            kv_namespaces: vec![CfKvNamespace {
                binding: "CACHE".to_owned(),
                id: "123".to_owned(),
                preview_id: Some("456".to_owned()),
            }],
            r2_buckets: vec![],
            d1_databases: vec![CfD1Database {
                binding: "DB".to_owned(),
                database_name: "app".to_owned(),
                database_id: "d1-id".to_owned(),
                preview_database_id: None,
            }],
            queues: CfQueues {
                producers: vec![CfQueueProducer {
                    binding: "JOBS".to_owned(),
                    queue: "jobs".to_owned(),
                }],
                consumers: vec![],
            },
            durable_objects: CfDurableObjects {
                bindings: vec![CfDurableBinding {
                    name: "STATE".to_owned(),
                    class_name: "State".to_owned(),
                    script_name: None,
                }],
                migrations: vec![CfDurableMigration {
                    tag: "v1".to_owned(),
                    new_classes: vec!["State".to_owned()],
                    new_sqlite_classes: Vec::new(),
                    deleted_classes: Vec::new(),
                    renamed_classes: Vec::new(),
                }],
            },
        };

        let rendered = render_wrangler(
            &section,
            &PathBuf::from("/tmp/app"),
            &PathBuf::from("/tmp/app/dist/worker.js"),
        )
        .expect("render");
        assert!(rendered.contains("name = \"skyzen-worker\""));
        assert!(rendered.contains("main = \"../../dist/worker.js\""));
        assert!(rendered.contains("[[d1_databases]]"));
        assert!(rendered.contains("binding = \"DB\""));
        assert!(rendered.contains("[[durable_objects.bindings]]"));
        assert!(rendered.contains("[[migrations]]"));
        assert!(rendered.contains("tag = \"v1\""));
        assert!(rendered.contains("new_classes = [\"State\"]"));
        assert!(rendered.contains("[vars]"));
    }

    #[test]
    fn renders_durable_object_script_name_and_sqlite_migrations() {
        let section = CloudflareSection {
            name: None,
            main: None,
            compatibility_date: Some("2025-02-01".to_owned()),
            compatibility_flags: Vec::new(),
            account_id: None,
            workers_dev: None,
            route: None,
            zone_id: None,
            vars: BTreeMap::new(),
            triggers: CfTriggers::default(),
            service: BTreeMap::new(),
            database: BTreeMap::new(),
            kv_namespaces: Vec::new(),
            r2_buckets: Vec::new(),
            d1_databases: Vec::new(),
            queues: CfQueues::default(),
            durable_objects: CfDurableObjects {
                bindings: vec![CfDurableBinding {
                    name: "STATE".to_owned(),
                    class_name: "SqliteState".to_owned(),
                    script_name: Some("shared-do-worker".to_owned()),
                }],
                migrations: vec![CfDurableMigration {
                    tag: "v2".to_owned(),
                    new_classes: Vec::new(),
                    new_sqlite_classes: vec!["SqliteState".to_owned()],
                    deleted_classes: vec!["LegacyState".to_owned()],
                    renamed_classes: vec![CfDurableRenamedClass {
                        from: "OldState".to_owned(),
                        to: "SqliteState".to_owned(),
                    }],
                }],
            },
        };

        let rendered = render_wrangler(
            &section,
            &PathBuf::from("/tmp/app"),
            &PathBuf::from("/tmp/app/dist/worker.js"),
        )
        .expect("render");
        assert!(rendered.contains("script_name = \"shared-do-worker\""));
        assert!(rendered.contains("new_sqlite_classes = [\"SqliteState\"]"));
        assert!(rendered.contains("deleted_classes = [\"LegacyState\"]"));
        assert!(
            rendered.contains("renamed_classes = [{ from = \"OldState\", to = \"SqliteState\" }]")
        );
    }

    #[test]
    fn renders_cron_triggers() {
        let section = CloudflareSection {
            compatibility_date: Some("2025-02-01".to_owned()),
            triggers: CfTriggers {
                crons: vec!["*/10 * * * *".to_owned()],
            },
            ..CloudflareSection::default()
        };

        let rendered = render_wrangler(
            &section,
            &PathBuf::from("/tmp/app"),
            &PathBuf::from("/tmp/app/dist/worker.js"),
        )
        .expect("render");
        assert!(rendered.contains("[triggers]"));
        assert!(rendered.contains("crons = [\"*/10 * * * *\"]"));
    }

    #[test]
    fn resolves_cloudflare_build_plan_from_cdylib_project() {
        let project_dir = temp_project_dir("cloudflare-build-plan");
        fs::write(
            project_dir.join("Cargo.toml"),
            r#"[package]
name = "demo-worker"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(project_dir.join("src")).expect("create src");
        fs::write(project_dir.join("src/lib.rs"), "pub fn demo() {}").expect("write lib.rs");

        let manifest = LoadedManifest {
            root_dir: project_dir.clone(),
            data: crate::manifest::SkyzenManifest {
                cloudflare: Some(CloudflareSection {
                    main: Some("dist/worker.js".to_owned()),
                    compatibility_date: Some("2025-02-01".to_owned()),
                    ..CloudflareSection::default()
                }),
                ..crate::manifest::SkyzenManifest::default()
            },
        };

        let build = resolve_build_plan(&manifest, manifest.data.cloudflare.as_ref().unwrap())
            .expect("resolve build plan");
        assert_eq!(build.entry_js_path, project_dir.join("dist/worker.js"));
        assert_eq!(build.bindings_js_path, project_dir.join("dist/worker_bg.js"));
        assert_eq!(build.wasm_output_path, project_dir.join("dist/worker_bg.wasm"));
        assert_eq!(build.bindgen_out_name, "worker");
        assert!(build.wasm_artifact_path.ends_with("wasm32-unknown-unknown/debug/demo_worker.wasm"));
    }

    #[test]
    fn rejects_cloudflare_build_without_cdylib() {
        let project_dir = temp_project_dir("cloudflare-build-invalid");
        fs::write(
            project_dir.join("Cargo.toml"),
            r#"[package]
name = "demo-worker"
version = "0.1.0"
edition = "2021"
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(project_dir.join("src")).expect("create src");
        fs::write(project_dir.join("src/lib.rs"), "pub fn demo() {}").expect("write lib.rs");

        let manifest = LoadedManifest {
            root_dir: project_dir,
            data: crate::manifest::SkyzenManifest {
                cloudflare: Some(CloudflareSection {
                    compatibility_date: Some("2025-02-01".to_owned()),
                    ..CloudflareSection::default()
                }),
                ..crate::manifest::SkyzenManifest::default()
            },
        };

        let error = resolve_build_plan(&manifest, manifest.data.cloudflare.as_ref().unwrap())
            .expect_err("missing cdylib should fail");
        assert!(error.to_string().contains("cdylib"));
    }

    #[test]
    fn prepare_cloudflare_includes_build_step_and_wrangler_command() {
        let project_dir = temp_project_dir("cloudflare-prepare");
        fs::write(
            project_dir.join("Cargo.toml"),
            r#"[package]
name = "demo-worker"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(project_dir.join("src")).expect("create src");
        fs::write(project_dir.join("src/lib.rs"), "pub fn demo() {}").expect("write lib.rs");

        let manifest = LoadedManifest {
            root_dir: project_dir,
            data: crate::manifest::SkyzenManifest {
                cloudflare: Some(CloudflareSection {
                    compatibility_date: Some("2025-02-01".to_owned()),
                    ..CloudflareSection::default()
                }),
                ..crate::manifest::SkyzenManifest::default()
            },
        };

        let plan = prepare(Action::Dev, &manifest).expect("prepare");
        assert_eq!(plan.commands.len(), 1);
        assert_eq!(plan.generated_files.len(), 1);
        assert_eq!(plan.internal_steps.len(), 1);
        assert!(plan.commands[0].display().contains("wrangler dev --local"));
        assert!(plan.generated_files[0].contents.contains("main = \"../../dist/worker.js\""));
    }

    fn temp_project_dir(prefix: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("skyzen-{prefix}-{suffix}"));
        fs::create_dir_all(&dir).expect("create temp project dir");
        dir
    }
}
