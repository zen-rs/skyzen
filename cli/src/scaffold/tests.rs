//! Scaffold tests.
//!
//! The compile tests are the gate for every template change: they generate each template into a
//! temporary directory, wire the workspace's own crates in by path, and run `cargo check` for both
//! the host and `wasm32-unknown-unknown`. That is materially stronger than asserting a generated
//! file contains a substring, and it is what catches template rot.

use super::{
    create_project, dependencies, template_files, validate_package_name, ExistingFiles,
    ScaffoldContext, ScaffoldRequest,
};
use crate::{
    cli::Template,
    providers::cloudflare::{build::embedded_wasm_bindgen_version, collect_local_durable_exports},
};
use skyzen_manifest::Manifest;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Every template, so a new one cannot be added without the whole suite covering it.
const ALL_TEMPLATES: &[(&str, Template)] = &[
    ("minimal", Template::Minimal),
    ("api", Template::Api),
    ("serverless-events", Template::ServerlessEvents),
    ("durable-realtime", Template::DurableRealtime),
];

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

/// A scaffold target inside `dir`.
///
/// `tempfile` names its directories `.tmpXXXXXX`, which is not a valid cargo package name — so
/// every test scaffolds into a named child rather than into the temporary directory itself.
fn project_dir(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("demo-app");
    fs::create_dir_all(&path).expect("create project dir");
    path
}

fn request(path: &Path, template: Template) -> ScaffoldRequest<'_> {
    ScaffoldRequest {
        path,
        template,
        existing: ExistingFiles::Refuse,
        dry_run: false,
        // Never in tests: `cargo add` talks to the network, and the workspace's own crates are
        // wired in by path below anyway.
        install_dependencies: false,
    }
}

fn context() -> ScaffoldContext {
    ScaffoldContext {
        package_name: "proj".to_owned(),
        worker_name: "proj".to_owned(),
        compatibility_date: "2026-01-01".to_owned(),
        wasm_bindgen_version: embedded_wasm_bindgen_version(),
    }
}

fn rendered_files(template: Template) -> Vec<(PathBuf, String)> {
    template_files(template, Path::new("proj"), &context()).expect("render")
}

fn rendered_manifest(template: Template, label: &str) -> String {
    rendered_files(template)
        .into_iter()
        .find(|(path, _)| path.ends_with("Skyzen.toml"))
        .unwrap_or_else(|| panic!("template `{label}` has no Skyzen.toml"))
        .1
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the cli crate has a workspace parent")
        .to_path_buf()
}

#[test]
fn every_template_generates_the_files_a_project_needs() {
    for (label, template) in ALL_TEMPLATES {
        let dir = temp_dir();
        let root = project_dir(&dir);
        create_project(&request(&root, *template))
            .unwrap_or_else(|error| panic!("template `{label}` failed to scaffold: {error:#}"));

        for expected in [
            "Cargo.toml",
            "Skyzen.toml",
            ".gitignore",
            ".env.example",
            "src/app.rs",
            "src/lib.rs",
            "src/main.rs",
        ] {
            assert!(
                root.join(expected).exists(),
                "template `{label}` did not write {expected}"
            );
        }

        let cargo_toml = fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");
        assert!(cargo_toml.contains("[lib]"), "template `{label}`");
        // Dependency versions come from `cargo add`, never from the template.
        assert!(
            !cargo_toml.contains("skyzen = \""),
            "template `{label}` hardcodes a skyzen version"
        );
        assert!(
            cargo_toml.contains(&format!(
                "wasm-bindgen = \"={}\"",
                embedded_wasm_bindgen_version()
            )),
            "template `{label}` must pin wasm-bindgen to the embedded generator"
        );
    }
}

#[test]
fn the_api_template_demonstrates_a_portable_service_end_to_end() {
    let dir = temp_dir();
    let root = project_dir(&dir);
    create_project(&request(&root, Template::Api)).expect("scaffold");

    let manifest = fs::read_to_string(root.join("Skyzen.toml")).expect("Skyzen.toml");
    assert!(manifest.contains("[[service]]"));
    assert!(manifest.contains("[native.service.cache]"));
    assert!(manifest.contains("[cloudflare.service.cache]"));
    assert!(manifest.contains("[[cloudflare.kv_namespaces]]"));

    // The generated extractor has to actually appear in a handler, or the manifest is decoration.
    let app = fs::read_to_string(root.join("src/app.rs")).expect("app.rs");
    assert!(app.contains("cache: Cache"), "{app}");
}

#[test]
fn the_serverless_and_durable_templates_keep_their_event_wiring() {
    let dir = temp_dir();
    let root = project_dir(&dir);
    create_project(&request(&root, Template::ServerlessEvents)).expect("scaffold");
    let manifest = fs::read_to_string(root.join("Skyzen.toml")).expect("Skyzen.toml");
    let lib = fs::read_to_string(root.join("src/lib.rs")).expect("lib.rs");
    assert!(manifest.contains("[[cloudflare.queues.consumers]]"));
    assert!(manifest.contains("[cloudflare.triggers]"));
    assert!(lib.contains("#[skyzen::queue]"));
    assert!(lib.contains("#[skyzen::scheduled]"));

    let dir = temp_dir();
    let root = project_dir(&dir);
    create_project(&request(&root, Template::DurableRealtime)).expect("scaffold");
    let manifest = fs::read_to_string(root.join("Skyzen.toml")).expect("Skyzen.toml");
    let durable =
        fs::read_to_string(root.join("src/durable_object.rs")).expect("durable_object.rs");
    assert!(manifest.contains("[[cloudflare.durable_objects.bindings]]"));
    assert!(manifest.contains("[[cloudflare.durable_objects.migrations]]"));
    assert!(durable.contains("#[skyzen::durable_object]"));
}

#[test]
fn every_template_manifest_parses_through_the_shared_schema() {
    for (label, template) in ALL_TEMPLATES {
        let contents = rendered_manifest(*template, label);
        let manifest = Manifest::parse(&contents, "Skyzen.toml", "proj")
            .unwrap_or_else(|error| panic!("template `{label}` Skyzen.toml failed: {error:#}"));
        assert!(
            manifest.cloudflare(None).expect("base").is_some(),
            "template `{label}` has no [cloudflare] section"
        );
    }
}

#[test]
fn every_template_manifest_survives_the_cloudflare_binding_cross_check() {
    // The api template wires a portable service to a KV binding; a template whose declarations and
    // bindings disagree would be rejected by `skyzen dev` on the user's first run.
    for (label, template) in ALL_TEMPLATES {
        let contents = rendered_manifest(*template, label);
        let manifest = Manifest::parse(&contents, "Skyzen.toml", "proj").expect("parses");
        let config = manifest
            .cloudflare(None)
            .expect("base")
            .expect("cloudflare section");
        let problems = crate::providers::cloudflare::binding_problems(manifest.data(), config);
        assert!(
            problems.is_empty(),
            "template `{label}` has binding problems: {problems:?}"
        );
    }
}

#[test]
fn every_template_manifest_renders_a_wrangler_config_wrangler_can_read() {
    use crate::providers::cloudflare::wrangler::{render, IdPolicy, RenderRequest};

    for (label, template) in ALL_TEMPLATES {
        let contents = rendered_manifest(*template, label);
        let manifest = Manifest::parse(&contents, "Skyzen.toml", "/tmp/proj").expect("parses");
        let base = manifest
            .cloudflare(None)
            .expect("base")
            .expect("cloudflare section");

        let rendered = render(&RenderRequest {
            base,
            environments: Vec::new(),
            root_dir: Path::new("/tmp/proj"),
            entry_js_path: Path::new("/tmp/proj/dist/worker.js"),
            wrangler_dir: PathBuf::from("/tmp/proj/.skyzen/gen"),
            // A scaffolded project has no provisioned ids yet, which is exactly what `dev` does.
            id_policy: IdPolicy::LocalPlaceholder,
        })
        .unwrap_or_else(|error| panic!("template `{label}` failed to render: {error:#}"));

        let parsed: toml::Table = toml::from_str(&rendered)
            .unwrap_or_else(|error| panic!("template `{label}` rendered invalid TOML: {error}"));
        assert_eq!(parsed["main"].as_str(), Some("../../dist/worker.js"));
        assert!(
            parsed.contains_key("compatibility_date"),
            "template `{label}`"
        );
    }
}

#[test]
fn every_template_declares_the_crates_its_code_uses() {
    // The capability check refuses to build a project whose manifest declares capabilities its
    // Cargo.toml has no crates for, so a template must list every crate its own manifest implies.
    for (label, template) in ALL_TEMPLATES {
        let contents = rendered_manifest(*template, label);
        let manifest = Manifest::parse(&contents, "Skyzen.toml", "proj").expect("parses");
        let declared: Vec<&str> = dependencies(*template)
            .iter()
            .map(|spec| spec.name)
            .collect();

        for capability in crate::capabilities::required(manifest.data()) {
            for requirement in capability.crates {
                assert!(
                    declared.contains(&requirement.name),
                    "template `{label}` needs `{}` for capability `{}` but does not add it",
                    requirement.name,
                    capability.name
                );
            }
        }
    }
}

#[test]
fn template_durable_class_names_match_rust_structs() {
    for (label, template) in ALL_TEMPLATES {
        let files = rendered_files(*template);
        let manifest = Manifest::parse(&rendered_manifest(*template, label), "Skyzen.toml", "proj")
            .expect("parses");
        let Some(cloudflare) = manifest.cloudflare(None).expect("base") else {
            continue;
        };

        let rust_sources: String = files
            .iter()
            .filter(|(path, _)| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
            .map(|(_, contents)| contents.as_str())
            .collect();

        for export in collect_local_durable_exports(cloudflare) {
            // `class_name` must be the Rust struct name: the macro exports `{Struct}Object` and
            // the worker shim re-exports it under the struct name, which is what wrangler sees.
            assert!(
                declares_struct(&rust_sources, &export.public_name),
                "template `{label}` declares durable class `{}` but no Rust source defines \
                 `struct {}`",
                export.public_name,
                export.public_name
            );
            assert_eq!(
                export.bindings_export_name,
                format!("{}Object", export.public_name)
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
                    "template `{label}` migration references `{class}` with no matching binding"
                );
            }
        }
    }
}

/// `source` declares `struct <name>` as a whole identifier (so `Room` does not match `RoomObject`).
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
fn a_project_path_of_dot_scaffolds_into_the_current_directory() {
    let dir = temp_dir();
    let nested = dir.path().join("my-app");
    fs::create_dir_all(&nested).expect("create dir");

    create_project(&request(&nested.join("."), Template::Minimal)).expect("scaffold");

    let cargo_toml = fs::read_to_string(nested.join("Cargo.toml")).expect("Cargo.toml");
    assert!(cargo_toml.contains("name = \"my-app\""), "{cargo_toml}");
}

#[test]
fn an_invalid_package_name_is_rejected_before_anything_is_written() {
    for name in ["my app", "9lives", "crate", "", "hello!"] {
        assert!(
            validate_package_name(name).is_err(),
            "`{name}` should be rejected"
        );
    }
    for name in ["my-app", "my_app", "_private", "app2"] {
        validate_package_name(name).unwrap_or_else(|error| panic!("`{name}`: {error}"));
    }

    let dir = temp_dir();
    let bad = dir.path().join("my app");
    fs::create_dir_all(&bad).expect("create dir");
    let error = create_project(&request(&bad, Template::Minimal)).expect_err("invalid name");
    assert!(error.to_string().contains("package name"), "{error}");
    assert!(
        !bad.join("Cargo.toml").exists(),
        "nothing should have been written"
    );
}

#[test]
fn a_non_empty_directory_needs_force_or_overwrite() {
    let dir = temp_dir();
    let root = project_dir(&dir);
    fs::write(root.join("Cargo.toml"), "# mine\n").expect("write");

    let error = create_project(&request(&root, Template::Minimal)).expect_err("not empty");
    assert!(error.to_string().contains("--force"), "{error}");
    assert!(error.to_string().contains("--overwrite"), "{error}");
}

#[test]
fn force_keeps_existing_files_and_overwrite_replaces_them() {
    let dir = temp_dir();
    let root = project_dir(&dir);
    let cargo_toml = root.join("Cargo.toml");
    fs::write(&cargo_toml, "# mine\n").expect("write");

    let mut forced = request(&root, Template::Minimal);
    forced.existing = ExistingFiles::Keep;
    create_project(&forced).expect("scaffold with --force");
    assert_eq!(
        fs::read_to_string(&cargo_toml).expect("Cargo.toml"),
        "# mine\n",
        "--force must not clobber an existing file"
    );
    // The files that were not already there are still written.
    assert!(root.join("src/app.rs").exists());

    let mut overwriting = request(&root, Template::Minimal);
    overwriting.existing = ExistingFiles::Replace;
    create_project(&overwriting).expect("scaffold with --overwrite");
    assert!(
        fs::read_to_string(&cargo_toml)
            .expect("Cargo.toml")
            .contains("[package]"),
        "--overwrite must replace it"
    );
}

#[test]
fn overwriting_a_dirty_git_worktree_is_refused() {
    let dir = temp_dir();
    let root = project_dir(&dir);
    let root = root.as_path();
    if !run_git(root, &["init"]) {
        // No usable git in this environment; the guard is unobservable, not broken.
        return;
    }
    fs::write(root.join("precious.txt"), "unsaved work\n").expect("write");

    let mut overwriting = request(root, Template::Minimal);
    overwriting.existing = ExistingFiles::Replace;
    let error = create_project(&overwriting).expect_err("dirty worktree");
    assert!(error.to_string().contains("uncommitted changes"), "{error}");
    assert!(root.join("precious.txt").exists());
}

fn run_git(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn the_env_example_lists_what_the_template_manifest_declares() {
    let dir = temp_dir();
    let root = project_dir(&dir);
    create_project(&request(&root, Template::Api)).expect("scaffold");
    let example = fs::read_to_string(root.join(".env.example")).expect(".env.example");
    // The api template's native backend is the in-process mock, so it needs no variables — and
    // the file says so rather than being empty and mysterious.
    assert!(
        example.contains("declares no native environment variables"),
        "{example}"
    );
}

#[test]
fn scaffolded_templates_compile() {
    let dir = temp_dir();
    let root = dir.path().to_path_buf();
    let shared_target_dir = root.join("target-cache");

    for (label, template) in ALL_TEMPLATES {
        compile_template(&root.join(label), *template, &shared_target_dir);
    }
}

fn compile_template(path: &Path, template: Template, shared_target_dir: &Path) {
    create_project(&request(path, template)).expect("template generation should succeed");
    wire_workspace_crates(path, template);
    run_cargo_check(path, shared_target_dir, false);
    run_cargo_check(path, shared_target_dir, true);
}

/// Point the generated project's Skyzen dependencies at this workspace.
///
/// In production these come from `cargo add`, which resolves them from the registry. A test must
/// not do that: it would need the network, and it would check the *published* crates rather than
/// the ones in this tree. Adding them by path checks what is about to be released, and the
/// `[patch.crates-io]` table catches any transitive request for a registry copy.
fn wire_workspace_crates(path: &Path, template: Template) {
    use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

    let workspace_root = workspace_root();
    let crate_dir = |name: &str| match name {
        "skyzen" => workspace_root.clone(),
        other => workspace_root.join(other.trim_start_matches("skyzen-")),
    };

    let cargo_toml_path = path.join("Cargo.toml");
    let existing = fs::read_to_string(&cargo_toml_path).expect("template Cargo.toml");
    let mut doc = existing
        .parse::<DocumentMut>()
        .expect("template Cargo.toml is valid TOML");

    let path_dependency = |dir: &Path, features: &[&str]| {
        let mut dependency = InlineTable::new();
        // Built with toml_edit rather than formatted by hand so Windows paths escape correctly —
        // `D:\a\skyzen` contains `\a` and `\s`, which a hand-written string would emit as invalid
        // TOML escapes.
        dependency.insert("path", Value::from(dir.display().to_string()));
        if !features.is_empty() {
            let mut array = toml_edit::Array::new();
            for feature in features {
                array.push(*feature);
            }
            dependency.insert("features", Value::Array(array));
        }
        Item::Value(Value::InlineTable(dependency))
    };

    let dependencies = doc["dependencies"].or_insert(Item::Table(Table::new()));
    for spec in super::dependencies(template) {
        if spec.name.starts_with("skyzen") {
            dependencies[spec.name] = path_dependency(&crate_dir(spec.name), spec.features);
        } else {
            // Third-party crates come from the registry, as they would for a real project.
            let mut dependency = InlineTable::new();
            dependency.insert("version", Value::from("*"));
            if !spec.features.is_empty() {
                let mut array = toml_edit::Array::new();
                for feature in spec.features {
                    array.push(*feature);
                }
                dependency.insert("features", Value::Array(array));
            }
            dependencies[spec.name] = Item::Value(Value::InlineTable(dependency));
        }
    }

    let mut crates_io = Table::new();
    for name in [
        "skyzen",
        "skyzen-cloudflare",
        "skyzen-services",
        "skyzen-test",
    ] {
        crates_io.insert(name, path_dependency(&crate_dir(name), &[]));
    }
    let mut patch_table = Table::new();
    patch_table.set_implicit(true);
    patch_table.insert("crates-io", Item::Table(crates_io));
    doc.insert("patch", Item::Table(patch_table));

    fs::write(&cargo_toml_path, doc.to_string()).expect("patched Cargo.toml");
}

fn run_cargo_check(path: &Path, shared_target_dir: &Path, wasm: bool) {
    // Nested wasm checks are already covered by the normal test run. Under cargo-llvm-cov, rustc
    // injects coverage instrumentation that the wasm target in this environment cannot link
    // because `profiler_builtins` is unavailable.
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
