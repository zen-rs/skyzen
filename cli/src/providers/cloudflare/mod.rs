//! The Cloudflare Workers provider.

pub mod build;
pub mod ids;
pub mod provision;
pub mod wrangler;

use crate::{
    cli::SecretCommand,
    project::{Project, WASM_TARGET},
    providers::{Action, CommandPlan, GeneratedFile, ProviderPlan, RunMode},
};
use anyhow::{Context, Result};
use build::{BuildPlan, DurableObjectExport};
use skyzen_manifest::{CloudflareSection, Manifest, ServiceType, SkyzenManifest};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    path::{Path, PathBuf},
};
use wrangler::{IdPolicy, RenderRequest};

/// Where the generated wrangler configuration lives, relative to the project root.
const WRANGLER_CONFIG_PATH: &str = ".skyzen/gen/wrangler.toml";

/// Build the plan for one Cloudflare action.
///
/// # Errors
///
/// Fails when the manifest has no `[cloudflare]` section, when its bindings do not line up with
/// the portable capabilities it declares, when the package cannot produce a `cdylib`, or when the
/// application's `wasm-bindgen` disagrees with the embedded generator.
pub fn prepare(
    action: &Action,
    manifest: &Manifest,
    project: &Project,
    environment: Option<&str>,
    dry_run: bool,
) -> Result<ProviderPlan> {
    let config = manifest
        .cloudflare(environment)?
        .ok_or_else(|| anyhow::anyhow!("missing [cloudflare] section in Skyzen.toml"))?;
    ensure_bindings_line_up(manifest.data(), config)?;

    let wrangler_path = manifest.root_dir().join(WRANGLER_CONFIG_PATH);
    let wrangler_dir = wrangler_path
        .parent()
        .context("the generated wrangler.toml has no parent directory")?
        .to_path_buf();

    let needs_artifacts = matches!(
        action,
        Action::Build { .. } | Action::Dev { .. } | Action::Deploy
    );
    if needs_artifacts {
        // Before `cargo build`, not after: a mismatch surfaces from wasm-bindgen as an opaque
        // schema-version error, and the whole point is to replace that with the remedy.
        build::check_wasm_bindgen_agreement(project)?;
    }

    let build = needs_artifacts
        .then(|| resolve_build_plan(action, manifest.root_dir(), project, config))
        .transpose()?;
    // Kept typed for the entry path below, and boxed into the plan for the CLI to run.
    let boxed_build = build
        .clone()
        .map(|plan| Box::new(plan) as Box<dyn crate::providers::ArtifactBuild>);

    let entry_js_path = build.as_ref().map_or_else(
        || manifest.root_dir().join(entry_relative_path(config)),
        |plan| plan.entry_js_path.clone(),
    );

    let environments = manifest
        .environment_names()
        .map(|name| {
            manifest
                .cloudflare(Some(name))
                .map(|section| (name.to_owned(), section.expect("a named environment")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let rendered = wrangler::render(&RenderRequest {
        base: manifest
            .cloudflare(None)?
            .ok_or_else(|| anyhow::anyhow!("missing [cloudflare] section in Skyzen.toml"))?,
        environments,
        root_dir: manifest.root_dir(),
        entry_js_path: &entry_js_path,
        wrangler_dir,
        id_policy: if matches!(action, Action::Deploy) {
            IdPolicy::RequireProvisioned
        } else {
            IdPolicy::LocalPlaceholder
        },
    })?;

    let database_names: Vec<String> = config
        .d1_databases
        .iter()
        .map(|entry| entry.database_name.clone())
        .collect();
    let commands = wrangler_commands(
        action,
        &wrangler_path,
        manifest.root_dir(),
        environment,
        dry_run,
        &database_names,
    )?;
    let run_mode = if matches!(action, Action::Dev { .. }) {
        // wrangler picks up the regenerated `dist/` on its own, so a source change re-runs the
        // build while `wrangler dev` keeps running — restarting it would drop local state and
        // rebind the port for no reason.
        RunMode::Rebuild
    } else {
        RunMode::Once
    };

    Ok(ProviderPlan {
        commands,
        generated_files: vec![GeneratedFile {
            path: wrangler_path,
            contents: rendered,
        }],
        build: boxed_build,
        run_mode,
        child_env: Vec::new(),
        watch_root: matches!(action, Action::Dev { .. }).then(|| manifest.root_dir().to_path_buf()),
        execute_despite_dry_run: matches!(action, Action::Deploy),
    })
}

/// The wrangler invocations one action needs.
fn wrangler_commands(
    action: &Action,
    wrangler_path: &Path,
    root_dir: &Path,
    environment: Option<&str>,
    dry_run: bool,
    databases: &[String],
) -> Result<Vec<CommandPlan>> {
    let config_args = || -> Result<Vec<String>> {
        let mut args = vec!["--config".to_owned(), path_string(wrangler_path)?];
        if let Some(environment) = environment {
            args.push("--env".to_owned());
            args.push(environment.to_owned());
        }
        Ok(args)
    };

    let command = |args: Vec<String>| CommandPlan {
        program: "wrangler".to_owned(),
        args,
        cwd: Some(root_dir.to_path_buf()),
    };

    let commands = match action {
        // `build` produces artifacts and stops; there is nothing for wrangler to do.
        Action::Build { .. } => Vec::new(),
        Action::Dev { runner_args } => {
            let mut args = vec!["dev".to_owned(), "--local".to_owned()];
            args.extend(config_args()?);
            args.extend(runner_args.iter().cloned());
            vec![command(args)]
        }
        // One `wrangler d1 migrations apply` per declared database. The migrations directory
        // comes from the generated config, so `migrations_dir` is set the same way every other
        // wrangler key is.
        Action::Migrate { local } => databases
            .iter()
            .map(|database| {
                let mut args = vec![
                    "d1".to_owned(),
                    "migrations".to_owned(),
                    "apply".to_owned(),
                    database.clone(),
                ];
                args.extend(config_args()?);
                args.push(if *local { "--local" } else { "--remote" }.to_owned());
                Ok(command(args))
            })
            .collect::<Result<Vec<_>>>()?,
        Action::Deploy => {
            let mut args = vec!["deploy".to_owned()];
            args.extend(config_args()?);
            if dry_run {
                // Unlike every other command, `deploy --dry-run` runs the real build and hands the
                // real bundle to wrangler; only the upload is skipped. The previous behaviour
                // skipped the build entirely, so it validated nothing.
                args.push("--dry-run".to_owned());
            }
            vec![command(args)]
        }
        Action::Logs { wrangler_args } => {
            let mut args = vec!["tail".to_owned()];
            args.extend(config_args()?);
            args.extend(wrangler_args.iter().cloned());
            vec![command(args)]
        }
        Action::Secret(SecretCommand::Set { name }) => {
            let mut args = vec!["secret".to_owned(), "put".to_owned(), name.clone()];
            args.extend(config_args()?);
            vec![command(args)]
        }
        Action::Secret(SecretCommand::List) => {
            let mut args = vec!["secret".to_owned(), "list".to_owned()];
            args.extend(config_args()?);
            vec![command(args)]
        }
    };

    Ok(commands)
}

/// Fail when a portable capability's Cloudflare wiring does not line up with its binding.
///
/// These were warnings printed to stderr and then ignored. The macro already refuses to compile
/// on the same condition, so a warning here only delayed the same failure until after the build.
///
/// # Errors
///
/// Fails listing every mismatch at once.
pub fn ensure_bindings_line_up(
    manifest: &SkyzenManifest,
    config: &CloudflareSection,
) -> Result<()> {
    let problems = binding_problems(manifest, config);
    if problems.is_empty() {
        return Ok(());
    }

    anyhow::bail!(
        "Skyzen.toml's portable capabilities do not line up with its Cloudflare bindings:\n{}",
        problems
            .iter()
            .map(|problem| format!("  {problem}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Every mismatch between the portable capabilities and the Cloudflare bindings.
pub fn binding_problems(manifest: &SkyzenManifest, config: &CloudflareSection) -> Vec<String> {
    let mut problems = Vec::new();

    for service in &manifest.service {
        let Some(wiring) = config.service.get(&service.name) else {
            problems.push(format!(
                "portable service `{}` has no [cloudflare.service.{}] wiring",
                service.name, service.name
            ));
            continue;
        };

        let (declared, section) = match service.service_type {
            ServiceType::Kv => (
                config
                    .kv_namespaces
                    .iter()
                    .any(|entry| entry.binding == wiring.binding),
                "[[cloudflare.kv_namespaces]]",
            ),
            ServiceType::Storage => (
                config
                    .r2_buckets
                    .iter()
                    .any(|entry| entry.binding == wiring.binding),
                "[[cloudflare.r2_buckets]]",
            ),
            ServiceType::Queue => (
                config
                    .queues
                    .producers
                    .iter()
                    .any(|entry| entry.binding == wiring.binding),
                "[[cloudflare.queues.producers]]",
            ),
        };

        if !declared {
            problems.push(format!(
                "portable service `{}` is wired to binding `{}`, which no {section} entry declares",
                service.name, wiring.binding
            ));
        }
    }

    for database in &manifest.database {
        let Some(wiring) = config.database.get(&database.name) else {
            problems.push(format!(
                "portable database `{}` has no [cloudflare.database.{}] wiring",
                database.name, database.name
            ));
            continue;
        };

        if !config
            .d1_databases
            .iter()
            .any(|entry| entry.binding == wiring.binding)
        {
            problems.push(format!(
                "portable database `{}` is wired to binding `{}`, which no [[cloudflare.d1_databases]] entry declares",
                database.name, wiring.binding
            ));
        }
    }

    problems
}

fn resolve_build_plan(
    action: &Action,
    root_dir: &Path,
    project: &Project,
    config: &CloudflareSection,
) -> Result<BuildPlan> {
    let target_name = project.cdylib_target_name()?;
    let entry_rel = entry_relative_path_checked(config)?;
    let output_dir = root_dir.join(entry_rel.parent().unwrap_or_else(|| Path::new("")));
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

    // Deploys ship optimized wasm; dev keeps fast debug builds.
    let release = matches!(action, Action::Deploy | Action::Build { release: true });
    let wasm_artifact_path = project
        .target_directory()
        .join(WASM_TARGET)
        .join(if release { "release" } else { "debug" })
        .join(format!("{}.wasm", target_name.replace('-', "_")));

    Ok(BuildPlan {
        root_dir: root_dir.to_path_buf(),
        cargo_manifest_path: project.manifest_path().to_path_buf(),
        wasm_artifact_path,
        output_dir: output_dir.clone(),
        entry_js_path: root_dir.join(&entry_rel),
        bindings_js_path: output_dir.join(format!("{entry_stem}_bg.js")),
        wasm_output_path: output_dir.join(format!("{entry_stem}_bg.wasm")),
        bindgen_out_name: entry_stem.to_owned(),
        durable_exports: collect_local_durable_exports(config),
        event_members: build::event_members(config),
        release,
        // Optimizing a debug build wastes the time it saves, so only released artifacts get it.
        optimize: release,
    })
}

fn entry_relative_path(config: &CloudflareSection) -> PathBuf {
    PathBuf::from(config.main.as_deref().unwrap_or("dist/worker.js"))
}

fn entry_relative_path_checked(config: &CloudflareSection) -> Result<PathBuf> {
    let path = entry_relative_path(config);
    if path.is_absolute() {
        anyhow::bail!(
            "cloudflare.main must be relative to the project root, got {}",
            path.display()
        );
    }
    if path.extension().and_then(OsStr::to_str) != Some("js") {
        anyhow::bail!(
            "cloudflare.main must point to a .js file, got {}",
            path.display()
        );
    }
    Ok(path)
}

/// The Durable Object classes this Worker itself defines.
///
/// A binding carrying `script_name` points at a class another Worker owns, so this Worker neither
/// exports it nor needs the Rust struct.
pub fn collect_local_durable_exports(config: &CloudflareSection) -> Vec<DurableObjectExport> {
    let mut exports = BTreeSet::new();
    for binding in &config.durable_objects.bindings {
        if binding.script_name.is_some() {
            continue;
        }
        exports.insert(DurableObjectExport {
            public_name: binding.class_name.clone(),
            bindings_export_name: format!("{}Object", binding.class_name),
        });
    }
    exports.into_iter().collect()
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{binding_problems, collect_local_durable_exports, wrangler_commands};
    use crate::{
        cli::SecretCommand,
        providers::{Action, CommandPlan},
    };
    use skyzen_manifest::Manifest;
    use std::path::Path;

    fn manifest(source: &str) -> Manifest {
        Manifest::parse(source, "Skyzen.toml", "/tmp/app").expect("valid manifest")
    }

    fn problems(source: &str) -> Vec<String> {
        let manifest = manifest(source);
        let config = manifest
            .cloudflare(None)
            .expect("base")
            .expect("cloudflare section");
        binding_problems(manifest.data(), config)
    }

    #[test]
    fn a_wired_service_whose_binding_exists_has_no_problems() {
        assert!(problems(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.service.cache]\nbinding = \"CACHE\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"abc\"\n"
        )
        .is_empty());
    }

    #[test]
    fn missing_wiring_and_missing_bindings_are_both_reported() {
        let found = problems(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [[service]]\nname = \"uploads\"\ntype = \"storage\"\n\n\
             [[database]]\nname = \"main\"\ntype = \"sql\"\n\n\
             [cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.service.uploads]\nbinding = \"UPLOADS\"\n\n\
             [cloudflare.database.main]\nbinding = \"DB\"\n",
        );

        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].contains("[cloudflare.service.cache]"), "{found:?}");
        assert!(found[1].contains("[[cloudflare.r2_buckets]]"), "{found:?}");
        assert!(
            found[2].contains("[[cloudflare.d1_databases]]"),
            "{found:?}"
        );
    }

    #[test]
    fn only_locally_defined_durable_classes_are_exported() {
        let manifest = manifest(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [[cloudflare.durable_objects.bindings]]\nname = \"LOCAL\"\nclass_name = \"State\"\n\n\
             [[cloudflare.durable_objects.bindings]]\nname = \"REMOTE\"\nclass_name = \"Remote\"\nscript_name = \"other\"\n",
        );
        let exports = collect_local_durable_exports(
            manifest.cloudflare(None).expect("base").expect("section"),
        );
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].public_name, "State");
        assert_eq!(exports[0].bindings_export_name, "StateObject");
    }

    fn planned(action: &Action, environment: Option<&str>, dry_run: bool) -> Vec<String> {
        wrangler_commands(
            action,
            Path::new("/tmp/app/.skyzen/gen/wrangler.toml"),
            Path::new("/tmp/app"),
            environment,
            dry_run,
            &["app".to_owned()],
        )
        .expect("commands")
        .iter()
        .map(CommandPlan::display)
        .collect()
    }

    fn rendered(action: &Action, environment: Option<&str>, dry_run: bool) -> String {
        planned(action, environment, dry_run)
            .into_iter()
            .next()
            .expect("one command")
    }

    #[test]
    fn every_wrangler_command_carries_the_generated_config_and_the_environment() {
        let dev = rendered(
            &Action::Dev {
                runner_args: vec!["--test-scheduled".to_owned()],
            },
            Some("staging"),
            false,
        );
        assert!(dev.contains("wrangler dev --local"), "{dev}");
        assert!(
            dev.contains("--config /tmp/app/.skyzen/gen/wrangler.toml"),
            "{dev}"
        );
        assert!(dev.contains("--env staging"), "{dev}");
        // wrangler-only flags reach wrangler rather than being rejected by the parser.
        assert!(dev.contains("--test-scheduled"), "{dev}");

        let logs = rendered(
            &Action::Logs {
                wrangler_args: vec!["--format".to_owned(), "json".to_owned()],
            },
            None,
            false,
        );
        assert!(logs.contains("wrangler tail"), "{logs}");
        assert!(logs.contains("--format json)"), "{logs}");

        let secret = rendered(
            &Action::Secret(SecretCommand::Set {
                name: "API_KEY".to_owned(),
            }),
            None,
            false,
        );
        assert!(secret.contains("wrangler secret put API_KEY"), "{secret}");
    }

    #[test]
    fn a_dry_run_deploy_becomes_a_wrangler_dry_run_rather_than_a_skipped_build() {
        let deploy = rendered(&Action::Deploy, None, true);
        assert!(deploy.contains("wrangler deploy"), "{deploy}");
        assert!(deploy.contains("--dry-run"), "{deploy}");
    }

    #[test]
    fn build_needs_no_wrangler_invocation() {
        assert!(planned(&Action::Build { release: false }, None, false).is_empty());
    }

    #[test]
    fn migrate_applies_to_every_declared_database_and_picks_a_target() {
        let remote = planned(&Action::Migrate { local: false }, None, false);
        assert_eq!(remote.len(), 1);
        assert!(
            remote[0].contains("wrangler d1 migrations apply app"),
            "{remote:?}"
        );
        assert!(remote[0].contains("--remote"), "{remote:?}");

        let local = planned(&Action::Migrate { local: true }, None, false);
        assert!(local[0].contains("--local"), "{local:?}");
    }
}
