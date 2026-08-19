use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use time::{macros::format_description, OffsetDateTime};

use crate::args::{CliOptions, Template};

const API_CARGO_TOML: &str = include_str!("../templates/api/Cargo.toml.tmpl");
const API_SKYZEN_TOML: &str = include_str!("../templates/api/Skyzen.toml.tmpl");
const API_GITIGNORE: &str = include_str!("../templates/api/gitignore.tmpl");
const API_APP_RS: &str = include_str!("../templates/api/src/app.rs.tmpl");
const API_LIB_RS: &str = include_str!("../templates/api/src/lib.rs.tmpl");
const API_MAIN_RS: &str = include_str!("../templates/api/src/main.rs.tmpl");
const EVENTS_CARGO_TOML: &str = include_str!("../templates/serverless-events/Cargo.toml.tmpl");
const EVENTS_SKYZEN_TOML: &str = include_str!("../templates/serverless-events/Skyzen.toml.tmpl");
const EVENTS_GITIGNORE: &str = include_str!("../templates/serverless-events/gitignore.tmpl");
const EVENTS_APP_RS: &str = include_str!("../templates/serverless-events/src/app.rs.tmpl");
const EVENTS_LIB_RS: &str = include_str!("../templates/serverless-events/src/lib.rs.tmpl");
const EVENTS_MAIN_RS: &str = include_str!("../templates/serverless-events/src/main.rs.tmpl");
const DURABLE_CARGO_TOML: &str = include_str!("../templates/durable-realtime/Cargo.toml.tmpl");
const DURABLE_SKYZEN_TOML: &str = include_str!("../templates/durable-realtime/Skyzen.toml.tmpl");
const DURABLE_GITIGNORE: &str = include_str!("../templates/durable-realtime/gitignore.tmpl");
const DURABLE_APP_RS: &str = include_str!("../templates/durable-realtime/src/app.rs.tmpl");
const DURABLE_LIB_RS: &str = include_str!("../templates/durable-realtime/src/lib.rs.tmpl");
const DURABLE_MAIN_RS: &str = include_str!("../templates/durable-realtime/src/main.rs.tmpl");
const DURABLE_OBJECT_RS: &str =
    include_str!("../templates/durable-realtime/src/durable_object.rs.tmpl");

pub fn create_project(options: &CliOptions) -> Result<()> {
    let path = options
        .path
        .as_ref()
        .context("project path is required for `skyzen new`")?;
    let package_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .context("project path must end with a valid directory name")?;

    ensure_target_dir(path, options.force)?;

    let compatibility_date = OffsetDateTime::now_utc()
        .format(&format_description!("[year]-[month]-[day]"))
        .context("failed to format compatibility date")?;
    let worker_name = package_name.replace('_', "-");

    let files = match options.template {
        Template::Api => api_template_files(path, package_name, &worker_name, &compatibility_date),
        Template::ServerlessEvents => {
            serverless_events_template_files(path, package_name, &worker_name, &compatibility_date)
        }
        Template::DurableRealtime => {
            durable_realtime_template_files(path, package_name, &worker_name, &compatibility_date)
        }
    };

    if options.dry_run {
        for (path, contents) in files {
            println!("[dry-run] write {}", path.display());
            println!("{contents}");
        }
        return Ok(());
    }

    for (path, contents) in files {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("[skyzen] wrote {}", path.display());
    }

    Ok(())
}

fn ensure_target_dir(path: &Path, force: bool) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
        return Ok(());
    }

    if force {
        return Ok(());
    }

    let mut entries =
        fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?;
    if entries.next().is_some() {
        anyhow::bail!(
            "target directory `{}` already exists and is not empty; pass --force to reuse it",
            path.display()
        );
    }

    Ok(())
}

fn api_template_files(
    root: &Path,
    package_name: &str,
    worker_name: &str,
    compatibility_date: &str,
) -> Vec<(PathBuf, String)> {
    vec![
        (
            root.join("Cargo.toml"),
            render(
                API_CARGO_TOML,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join("Skyzen.toml"),
            render(
                API_SKYZEN_TOML,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join(".gitignore"),
            render(API_GITIGNORE, package_name, worker_name, compatibility_date),
        ),
        (
            root.join("src/app.rs"),
            render(API_APP_RS, package_name, worker_name, compatibility_date),
        ),
        (
            root.join("src/lib.rs"),
            render(API_LIB_RS, package_name, worker_name, compatibility_date),
        ),
        (
            root.join("src/main.rs"),
            render(API_MAIN_RS, package_name, worker_name, compatibility_date),
        ),
    ]
}

fn serverless_events_template_files(
    root: &Path,
    package_name: &str,
    worker_name: &str,
    compatibility_date: &str,
) -> Vec<(PathBuf, String)> {
    vec![
        (
            root.join("Cargo.toml"),
            render(
                EVENTS_CARGO_TOML,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join("Skyzen.toml"),
            render(
                EVENTS_SKYZEN_TOML,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join(".gitignore"),
            render(
                EVENTS_GITIGNORE,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join("src/app.rs"),
            render(EVENTS_APP_RS, package_name, worker_name, compatibility_date),
        ),
        (
            root.join("src/lib.rs"),
            render(EVENTS_LIB_RS, package_name, worker_name, compatibility_date),
        ),
        (
            root.join("src/main.rs"),
            render(
                EVENTS_MAIN_RS,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
    ]
}

fn durable_realtime_template_files(
    root: &Path,
    package_name: &str,
    worker_name: &str,
    compatibility_date: &str,
) -> Vec<(PathBuf, String)> {
    vec![
        (
            root.join("Cargo.toml"),
            render(
                DURABLE_CARGO_TOML,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join("Skyzen.toml"),
            render(
                DURABLE_SKYZEN_TOML,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join(".gitignore"),
            render(
                DURABLE_GITIGNORE,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join("src/app.rs"),
            render(
                DURABLE_APP_RS,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join("src/durable_object.rs"),
            render(
                DURABLE_OBJECT_RS,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join("src/lib.rs"),
            render(
                DURABLE_LIB_RS,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
        (
            root.join("src/main.rs"),
            render(
                DURABLE_MAIN_RS,
                package_name,
                worker_name,
                compatibility_date,
            ),
        ),
    ]
}

fn render(
    template: &str,
    package_name: &str,
    worker_name: &str,
    compatibility_date: &str,
) -> String {
    template
        .replace("{{PACKAGE_NAME}}", package_name)
        .replace("{{WORKER_NAME}}", worker_name)
        .replace("{{COMPATIBILITY_DATE}}", compatibility_date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Action, CliOptions, Template};
    use std::process::Command;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    #[test]
    fn creates_api_template() {
        let dir = temp_dir();
        let path = dir.path().to_path_buf();
        let options = CliOptions {
            action: Action::New,
            provider: None,
            manifest: PathBuf::from("Skyzen.toml"),
            dry_run: false,
            template: Template::Api,
            path: Some(path.clone()),
            force: false,
        };

        create_project(&options).expect("scaffold should succeed");

        let cargo_toml = fs::read_to_string(path.join("Cargo.toml")).expect("Cargo.toml");
        let manifest = fs::read_to_string(path.join("Skyzen.toml")).expect("Skyzen.toml");
        let app = fs::read_to_string(path.join("src/app.rs")).expect("app.rs");
        assert!(cargo_toml.contains("[lib]"));
        assert!(manifest.contains("[cloudflare]"));
        assert!(!app.contains("{{PACKAGE_NAME}}"));
    }

    #[test]
    fn creates_serverless_events_template() {
        let dir = temp_dir();
        let path = dir.path().to_path_buf();
        let options = CliOptions {
            action: Action::New,
            provider: None,
            manifest: PathBuf::from("Skyzen.toml"),
            dry_run: false,
            template: Template::ServerlessEvents,
            path: Some(path.clone()),
            force: false,
        };

        create_project(&options).expect("scaffold should succeed");

        let cargo_toml = fs::read_to_string(path.join("Cargo.toml")).expect("Cargo.toml");
        let manifest = fs::read_to_string(path.join("Skyzen.toml")).expect("Skyzen.toml");
        let lib_rs = fs::read_to_string(path.join("src/lib.rs")).expect("lib.rs");
        assert!(cargo_toml.contains("skyzen-cloudflare"));
        assert!(manifest.contains("[[cloudflare.queues.consumers]]"));
        assert!(manifest.contains("[cloudflare.triggers]"));
        assert!(lib_rs.contains("#[skyzen::queue]"));
        assert!(lib_rs.contains("#[skyzen::scheduled]"));
    }

    #[test]
    fn creates_durable_realtime_template() {
        let dir = temp_dir();
        let path = dir.path().to_path_buf();
        let options = CliOptions {
            action: Action::New,
            provider: None,
            manifest: PathBuf::from("Skyzen.toml"),
            dry_run: false,
            template: Template::DurableRealtime,
            path: Some(path.clone()),
            force: false,
        };

        create_project(&options).expect("scaffold should succeed");

        let manifest = fs::read_to_string(path.join("Skyzen.toml")).expect("Skyzen.toml");
        let durable =
            fs::read_to_string(path.join("src/durable_object.rs")).expect("durable_object.rs");
        assert!(manifest.contains("[[cloudflare.durable_objects.bindings]]"));
        assert!(manifest.contains("[[cloudflare.durable_objects.migrations]]"));
        assert!(durable.contains("#[skyzen::durable_object]"));
    }

    fn all_template_files() -> Vec<(&'static str, Vec<(PathBuf, String)>)> {
        let root = Path::new("proj");
        vec![
            (
                "api",
                api_template_files(root, "proj", "proj", "2026-01-01"),
            ),
            (
                "serverless-events",
                serverless_events_template_files(root, "proj", "proj", "2026-01-01"),
            ),
            (
                "durable-realtime",
                durable_realtime_template_files(root, "proj", "proj", "2026-01-01"),
            ),
        ]
    }

    fn template_manifest(files: &[(PathBuf, String)], template: &str) -> String {
        files
            .iter()
            .find(|(path, _)| path.ends_with("Skyzen.toml"))
            .unwrap_or_else(|| panic!("template `{template}` has no Skyzen.toml"))
            .1
            .clone()
    }

    /// `source` declares `struct <name>` as a whole identifier (so `Room`
    /// does not match `RoomObject`).
    fn declares_struct(source: &str, name: &str) -> bool {
        source
            .match_indices(&format!("struct {name}"))
            .any(|(index, matched)| {
                !matches!(
                    source[index + matched.len()..].chars().next(),
                    Some(c) if c.is_alphanumeric() || c == '_'
                )
            })
    }

    #[test]
    fn every_template_manifest_parses() {
        let dir = temp_dir();
        for (template, files) in all_template_files() {
            let contents = template_manifest(&files, template);
            let manifest_path = dir.path().join(format!("{template}-Skyzen.toml"));
            fs::write(&manifest_path, contents).expect("write Skyzen.toml");

            let loaded =
                crate::manifest::LoadedManifest::load(&manifest_path).unwrap_or_else(|error| {
                    panic!("template `{template}` Skyzen.toml failed to parse: {error:#}")
                });
            assert!(
                loaded.data.cloudflare.is_some(),
                "template `{template}` Skyzen.toml is missing the [cloudflare] section"
            );
        }
    }

    #[test]
    fn template_durable_class_names_match_rust_structs() {
        use crate::providers::cloudflare::collect_local_durable_exports;

        for (template, files) in all_template_files() {
            let manifest: crate::manifest::SkyzenManifest =
                toml::from_str(&template_manifest(&files, template)).unwrap_or_else(|error| {
                    panic!("template `{template}` Skyzen.toml failed to parse: {error}")
                });
            let Some(cloudflare) = manifest.cloudflare else {
                continue;
            };

            let rust_sources: String = files
                .iter()
                .filter(|(path, _)| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
                .map(|(_, contents)| contents.as_str())
                .collect();

            for export in collect_local_durable_exports(&cloudflare) {
                // `class_name` must be the Rust struct name: the macro exports
                // `{Struct}Object` and the worker shim re-exports it under the
                // struct name, which is what wrangler references.
                assert!(
                    declares_struct(&rust_sources, &export.public_name),
                    "template `{template}` declares durable class `{}` but no Rust source \
                     defines `struct {}`",
                    export.public_name,
                    export.public_name
                );
                assert_eq!(
                    export.bindings_export_name,
                    format!("{}Object", export.public_name),
                    "shim export name must match the `{{Struct}}Object` wasm export"
                );
            }

            for migration in &cloudflare.durable_objects.migrations {
                for class in migration
                    .new_classes
                    .iter()
                    .chain(&migration.new_sqlite_classes)
                {
                    assert!(
                        cloudflare
                            .durable_objects
                            .bindings
                            .iter()
                            .any(|binding| binding.class_name == *class),
                        "template `{template}` migration references `{class}` which has no \
                         matching durable object binding"
                    );
                }
            }
        }
    }

    #[test]
    fn scaffolded_templates_compile() {
        let dir = temp_dir();
        let root = dir.path().to_path_buf();
        let shared_target_dir = root.join("target-cache");

        compile_template(
            &root.join("api-app"),
            Template::Api,
            &shared_target_dir,
            true,
        );
        compile_template(
            &root.join("events-app"),
            Template::ServerlessEvents,
            &shared_target_dir,
            true,
        );
        compile_template(
            &root.join("durable-app"),
            Template::DurableRealtime,
            &shared_target_dir,
            true,
        );
    }

    fn compile_template(
        path: &Path,
        template: Template,
        shared_target_dir: &Path,
        check_wasm: bool,
    ) {
        let options = CliOptions {
            action: Action::New,
            provider: None,
            manifest: PathBuf::from("Skyzen.toml"),
            dry_run: false,
            template,
            path: Some(path.to_path_buf()),
            force: false,
        };

        create_project(&options).expect("template generation should succeed");
        patch_local_crates(path);
        run_cargo_check(path, shared_target_dir, false);
        if check_wasm {
            run_cargo_check(path, shared_target_dir, true);
        }
    }

    fn patch_local_crates(path: &Path) {
        use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("cli crate should have a workspace parent")
            .to_path_buf();

        let cargo_toml_path = path.join("Cargo.toml");
        let existing = fs::read_to_string(&cargo_toml_path).expect("template Cargo.toml");
        let mut doc = existing
            .parse::<DocumentMut>()
            .expect("template Cargo.toml is valid TOML");

        // Build `[patch.crates-io]` via toml_edit so paths are escaped correctly — a hand-formatted
        // string would emit invalid TOML for Windows paths (e.g. `D:\a\skyzen` has `\a`/`\s`).
        let mut crates_io = Table::new();
        for (name, dir) in [
            ("skyzen", workspace_root.clone()),
            ("skyzen-cloudflare", workspace_root.join("cloudflare")),
            ("skyzen-services", workspace_root.join("services")),
        ] {
            let mut dep = InlineTable::new();
            dep.insert("path", Value::from(dir.display().to_string()));
            crates_io.insert(name, Item::Value(Value::InlineTable(dep)));
        }

        let mut patch_table = Table::new();
        patch_table.set_implicit(true);
        patch_table.insert("crates-io", Item::Table(crates_io));
        doc.insert("patch", Item::Table(patch_table));

        fs::write(&cargo_toml_path, doc.to_string()).expect("patched Cargo.toml");
    }

    fn run_cargo_check(path: &Path, shared_target_dir: &Path, wasm: bool) {
        // Nested wasm checks are already covered by the normal test run. Under cargo-llvm-cov,
        // rustc injects coverage instrumentation that the wasm target in this environment cannot
        // link because `profiler_builtins` is unavailable.
        if wasm && cfg!(coverage) {
            return;
        }

        let mut command = Command::new("cargo");
        // Don't pass `--offline`: the generated projects (especially the wasm Cloudflare templates)
        // pull deps such as `gloo-timers` that the workspace's native build never caches, so an
        // offline check fails on CI even though the template is correct. Let cargo fetch as needed.
        command.arg("check").arg("--quiet");
        if wasm {
            command.arg("--target").arg("wasm32-unknown-unknown");
        }
        let output = command
            .current_dir(path)
            .env("CARGO_TARGET_DIR", shared_target_dir)
            .env("RUSTFLAGS", "")
            .env("CARGO_ENCODED_RUSTFLAGS", "")
            .env("RUSTDOCFLAGS", "")
            .env("CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS", "")
            .env_remove("LLVM_PROFILE_FILE")
            .output()
            .expect("cargo check should launch");

        assert!(
            output.status.success(),
            "cargo check failed for {} (wasm={}):\nstdout:\n{}\nstderr:\n{}",
            path.display(),
            wasm,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
