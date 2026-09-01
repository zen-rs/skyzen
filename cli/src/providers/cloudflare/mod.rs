//! The Cloudflare Workers provider.

pub mod build;
pub mod ids;
pub mod provision;
mod secrets;
pub mod wrangler;

use crate::{
    project::{Project, WASM_TARGET},
    providers::{
        Action, CommandPlan, CommandStdin, FileContents, GeneratedFile, ProviderPlan, RunMode,
        SecretAction, Step,
    },
};

use anyhow::{Context, Result};
use build::{BuildPlan, DurableObjectExport};
use secrets::SecretDelivery;
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
    let wrangler_dir_path = wrangler_path
        .parent()
        .context("the generated wrangler.toml has no parent directory")?
        .to_path_buf();
    let wrangler_dir = wrangler_dir_path.clone();

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
        .map(|plan| Box::new(plan) as Box<dyn crate::providers::Task>);

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
    let planned = plan_work(&WorkRequest {
        action,
        manifest,
        config,
        wrangler_path: &wrangler_path,
        wrangler_dir: &wrangler_dir_path,
        environment,
        dry_run,
        databases: &database_names,
    })?;
    let run_mode = if matches!(action, Action::Dev { .. }) {
        // wrangler picks up the regenerated `dist/` on its own, so a source change re-runs the
        // build while `wrangler dev` keeps running — restarting it would drop local state and
        // rebind the port for no reason.
        RunMode::Rebuild
    } else {
        RunMode::Once
    };

    let mut generated_files = vec![GeneratedFile {
        path: wrangler_path,
        contents: FileContents::Public(rendered),
    }];
    generated_files.extend(planned.generated_files);

    Ok(ProviderPlan {
        steps: planned.steps,
        generated_files,
        build: boxed_build,
        run_mode,
        child_env: Vec::new(),
        watch_root: matches!(action, Action::Dev { .. }).then(|| manifest.root_dir().to_path_buf()),
        execute_despite_dry_run: matches!(action, Action::Deploy),
    })
}

/// Everything the work for one action is planned from.
#[derive(Debug)]
struct WorkRequest<'a> {
    /// What was asked for.
    action: &'a Action,
    /// The project.
    manifest: &'a Manifest,
    /// The selected environment's Cloudflare section.
    config: &'a CloudflareSection,
    /// The generated wrangler configuration every invocation is pointed at.
    wrangler_path: &'a Path,
    /// Its directory, which is where wrangler resolves `.dev.vars` against.
    wrangler_dir: &'a Path,
    /// The `--env` the user selected, if any.
    environment: Option<&'a str>,
    /// Whether this is a dry run.
    dry_run: bool,
    /// The D1 databases the manifest declares, for `migrate`.
    databases: &'a [String],
}

/// The work one action performs, and the files it needs written first.
#[derive(Debug, Default)]
struct PlannedWork {
    /// The steps, in order.
    steps: Vec<Step>,
    /// Files beyond the generated `wrangler.toml`.
    generated_files: Vec<GeneratedFile>,
}

impl PlannedWork {
    /// The plan for an action that only runs commands.
    fn from_commands(commands: Vec<CommandPlan>) -> Self {
        Self {
            steps: commands.into_iter().map(Step::Command).collect(),
            generated_files: Vec::new(),
        }
    }
}

/// The wrangler invocations and local work one action needs.
fn plan_work(request: &WorkRequest<'_>) -> Result<PlannedWork> {
    let config_path = path_string(request.wrangler_path)?;
    let mut config_args = vec!["--config".to_owned(), config_path.clone()];
    if let Some(environment) = request.environment {
        config_args.push("--env".to_owned());
        config_args.push(environment.to_owned());
    }
    let root_dir = request.manifest.root_dir();

    let command = |head: &[&str], extra: &[String]| {
        let mut args: Vec<String> = head.iter().map(|part| (*part).to_owned()).collect();
        args.extend(config_args.iter().cloned());
        args.extend(extra.iter().cloned());
        CommandPlan {
            program: "wrangler".to_owned(),
            args,
            cwd: Some(root_dir.to_path_buf()),
            stdin: CommandStdin::Inherit,
        }
    };

    let secrets = SecretDelivery {
        manifest: request.manifest,
        config: request.config,
        config_args: &config_args,
        config_path: &config_path,
        cwd: root_dir,
    };

    let work = match request.action {
        // `build` produces artifacts and stops; there is nothing for wrangler to do.
        Action::Build { .. } => PlannedWork::default(),
        Action::Dev { runner_args } => {
            // Every declared secret, store-backed ones included: `wrangler dev` serves from a
            // local store that starts out empty however full the account's is.
            let resolved = secrets.all()?;
            let (mut steps, dev_vars) = secrets.local_work(&resolved, request.dev_vars_path())?;
            steps.push(Step::Command(command(&["dev", "--local"], runner_args)));
            PlannedWork {
                steps,
                generated_files: dev_vars.into_iter().collect(),
            }
        }
        // One `wrangler d1 migrations` invocation per declared database — `apply` to run the
        // pending files, `list` to report them, which is D1's own `migrate status`. The migrations
        // directory comes from the generated config, so `migrations_dir` is set the same way every
        // other wrangler key is.
        Action::Migrate { local, status } => PlannedWork::from_commands(
            request
                .databases
                .iter()
                .map(|database| {
                    command(
                        &[
                            "d1",
                            "migrations",
                            if *status { "list" } else { "apply" },
                            database,
                        ],
                        &[if *local { "--local" } else { "--remote" }.to_owned()],
                    )
                })
                .collect(),
        ),
        Action::Deploy => {
            // Resolved before anything is uploaded, and under `--dry-run` too: a deployment that
            // ships half its configuration fails at cold start, where the message is a panic in a
            // log rather than a list of manifest entries.
            let resolved = secrets.classic()?;
            let mut commands = vec![command(
                &["deploy"],
                &if request.dry_run {
                    // Unlike every other command, `deploy --dry-run` runs the real build and hands
                    // the real bundle to wrangler; only the upload is skipped.
                    vec!["--dry-run".to_owned()]
                } else {
                    Vec::new()
                },
            )];
            // Values are attached to the Worker the upload just created, so there is nothing to
            // attach them to when nothing was uploaded.
            if !request.dry_run {
                commands.extend(secrets.bulk_step(&resolved)?);
            }
            PlannedWork::from_commands(commands)
        }
        Action::Logs { wrangler_args } => {
            PlannedWork::from_commands(vec![command(&["tail"], wrangler_args)])
        }
        Action::Secret(SecretAction::Set { name, value }) => PlannedWork {
            steps: vec![secrets.set_step(name, value)],
            generated_files: Vec::new(),
        },
        Action::Secret(SecretAction::Push) => {
            let resolved = secrets.classic()?;
            let bulk = secrets.bulk_step(&resolved)?.context(
                "there is no classic [[secret]] to push. A `[cloudflare.secret.<NAME>]` entry is \
                 backed by Secrets Store, which is externally managed: write one with `skyzen \
                 secret set <NAME>`",
            )?;
            PlannedWork::from_commands(vec![bulk])
        }
        Action::Secret(SecretAction::List) => {
            PlannedWork::from_commands(vec![command(&["secret", "list"], &[])])
        }
    };

    Ok(work)
}

impl WorkRequest<'_> {
    /// Where `wrangler dev` reads local values from.
    ///
    /// Beside the generated configuration, because that is what wrangler resolves the name
    /// against. With `--env`, the suffixed name: wrangler loads *only* `.dev.vars.<env>` when it
    /// exists, so writing the plain name under a named environment would be read by nothing.
    fn dev_vars_path(&self) -> PathBuf {
        self.wrangler_dir.join(self.environment.map_or_else(
            || ".dev.vars".to_owned(),
            |environment| format!(".dev.vars.{environment}"),
        ))
    }
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
    use super::{
        binding_problems, collect_local_durable_exports, plan_work, PlannedWork, WorkRequest,
    };
    use crate::{
        environment,
        providers::{Action, CommandPlan, CommandStdin, FileContents, SecretAction, Step},
    };
    use secrecy::SecretString;
    use skyzen_manifest::{Manifest, VarName};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn manifest(source: &str) -> Manifest {
        Manifest::parse(source, "Skyzen.toml", "/tmp/app").expect("valid manifest")
    }

    /// A `[cloudflare]` section with nothing in it but the key every manifest needs, and a named
    /// environment for the tests that select one.
    const SECTION: &str =
        "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n[cloudflare.env.staging]\n";

    /// A project whose root holds `dotenv`, so a declared secret resolves without the test having
    /// to mutate the process environment (which every other test in the binary shares).
    fn project(source: &str, dotenv: &str) -> (TempDir, Manifest) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(".env"), dotenv).expect("write .env");
        let manifest = Manifest::parse(source, dir.path().join("Skyzen.toml"), dir.path())
            .expect("valid manifest");
        (dir, manifest)
    }

    /// The work one action plans, as the CLI would plan it.
    fn work(
        action: &Action,
        manifest: &Manifest,
        environment: Option<&str>,
        dry_run: bool,
    ) -> anyhow::Result<PlannedWork> {
        let wrangler_path = manifest.root_dir().join(super::WRANGLER_CONFIG_PATH);
        let wrangler_dir: PathBuf = wrangler_path.parent().expect("a parent").to_path_buf();
        let config = manifest
            .cloudflare(environment)
            .expect("base")
            .expect("cloudflare section");
        plan_work(&WorkRequest {
            action,
            manifest,
            config,
            wrangler_path: &wrangler_path,
            wrangler_dir: &wrangler_dir,
            environment,
            dry_run,
            databases: &["app".to_owned()],
        })
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
        let found = problems(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.service.cache]\nbinding = \"CACHE\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"abc\"\n",
        );
        assert!(found.is_empty(), "{found:?}");
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
        work(action, &manifest(SECTION), environment, dry_run)
            .expect("commands")
            .steps
            .iter()
            .map(Step::describe)
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
        // Joined the way the plan joins it, so the separator is the host's on every platform.
        let config = PathBuf::from("/tmp/app").join(super::WRANGLER_CONFIG_PATH);
        assert!(
            dev.contains(&format!("--config {}", config.display())),
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

        let list = rendered(&Action::Secret(SecretAction::List), None, false);
        assert!(list.contains("wrangler secret list"), "{list}");
    }

    #[test]
    fn a_dry_run_deploy_becomes_a_wrangler_dry_run_rather_than_a_skipped_build() {
        let deploy = rendered(&Action::Deploy, None, true);
        assert!(deploy.contains("wrangler deploy"), "{deploy}");
        assert!(deploy.contains("--dry-run"), "{deploy}");
    }

    #[test]
    fn build_needs_no_wrangler_invocation() {
        let commands = planned(&Action::Build { release: false }, None, false);
        assert!(commands.is_empty(), "{commands:?}");
    }

    #[test]
    fn migrate_applies_to_every_declared_database_and_picks_a_target() {
        let remote = planned(
            &Action::Migrate {
                local: false,
                status: false,
            },
            None,
            false,
        );
        assert_eq!(remote.len(), 1);
        assert!(
            remote[0].contains("wrangler d1 migrations apply app"),
            "{remote:?}"
        );
        assert!(remote[0].contains("--remote"), "{remote:?}");

        let local = planned(
            &Action::Migrate {
                local: true,
                status: false,
            },
            None,
            false,
        );
        assert!(local[0].contains("--local"), "{local:?}");
    }

    #[test]
    fn migrate_status_lists_rather_than_applying() {
        let status = planned(
            &Action::Migrate {
                local: false,
                status: true,
            },
            None,
            false,
        );
        assert_eq!(status.len(), 1);
        assert!(
            status[0].contains("wrangler d1 migrations list app"),
            "{status:?}"
        );
        assert!(status[0].contains("--remote"), "{status:?}");
    }

    /// A declared secret, and the same secret backed by a Secrets Store entry.
    const CLASSIC: &str = "[[secret]]\nname = \"STRIPE_KEY\"\n\n[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
         [cloudflare.env.staging]\n";
    const STORE_BACKED: &str = "[[secret]]\nname = \"STRIPE_KEY\"\n\n[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
         [cloudflare.secret.STRIPE_KEY]\nstore_id = \"store_1\"\nsecret_name = \"stripe-key\"\n";

    /// The commands of a plan, with what each carries on its standard input.
    fn commands(work: &PlannedWork) -> Vec<&CommandPlan> {
        work.steps
            .iter()
            .filter_map(|step| match step {
                Step::Command(command) => Some(command),
                Step::Task(_) => None,
            })
            .collect()
    }

    /// What a command was given to write to the process's standard input.
    fn stdin(command: &CommandPlan) -> Option<&str> {
        match &command.stdin {
            CommandStdin::Inherit => None,
            CommandStdin::Secret(value) => Some(environment::expose(value)),
        }
    }

    #[test]
    fn a_deploy_delivers_the_declared_secrets_after_the_upload() {
        let (_dir, manifest) = project(CLASSIC, "STRIPE_KEY=sk_live_123\n");
        let planned = work(&Action::Deploy, &manifest, None, false).expect("plan");

        let commands = commands(&planned);
        assert_eq!(commands.len(), 2, "{planned:?}");
        assert!(commands[0].display().contains("wrangler deploy"));
        // After the upload: the values are attached to the Worker it created.
        assert!(
            commands[1].display().contains("wrangler secret bulk"),
            "{:?}",
            commands[1].display()
        );
        assert_eq!(stdin(commands[1]), Some(r#"{"STRIPE_KEY":"sk_live_123"}"#));
        // The whole point of standard input: the rendering carries none of it.
        assert!(!commands[1].display().contains("sk_live_123"));
    }

    #[test]
    fn a_deploy_refuses_when_a_declared_secret_is_set_nowhere() {
        let (_dir, manifest) = project(CLASSIC, "");
        let error = work(&Action::Deploy, &manifest, None, false)
            .expect_err("the value is set nowhere, and a Worker without it panics at cold start");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("STRIPE_KEY"), "{rendered}");
        assert!(rendered.contains("[[secret]] STRIPE_KEY"), "{rendered}");
    }

    #[test]
    fn a_deploy_needs_neither_a_store_backed_secret_nor_a_native_wiring_variable() {
        // A Secrets Store entry is bound rather than uploaded, and a Worker resolves its services
        // from bindings — so demanding either value would refuse a complete deployment.
        let source = format!(
            "{STORE_BACKED}\n[[database]]\nname = \"main\"\ntype = \"sql\"\n\n\
             [native.database.main]\nbackend = \"postgres\"\nurl_env = \"JOURNAL_URL\"\n"
        );
        let (_dir, manifest) = project(&source, "");
        let planned = work(&Action::Deploy, &manifest, None, false).expect("plan");

        let commands = commands(&planned);
        assert_eq!(commands.len(), 1, "nothing to deliver: {planned:?}");
        assert!(commands[0].display().contains("wrangler deploy"));
    }

    #[test]
    fn a_dry_run_deploy_uploads_nothing_and_so_delivers_nothing() {
        let (_dir, manifest) = project(CLASSIC, "STRIPE_KEY=sk_live_123\n");
        let planned = work(&Action::Deploy, &manifest, None, true).expect("plan");

        let commands = commands(&planned);
        assert_eq!(commands.len(), 1, "{planned:?}");
        assert!(commands[0].display().contains("--dry-run"));
    }

    #[test]
    fn dev_writes_the_declared_secrets_where_wrangler_reads_them() {
        let (_dir, manifest) = project(CLASSIC, "STRIPE_KEY=sk_live_123\n");
        let planned = work(
            &Action::Dev {
                runner_args: Vec::new(),
            },
            &manifest,
            None,
            false,
        )
        .expect("plan");

        assert_eq!(planned.generated_files.len(), 1);
        let file = &planned.generated_files[0];
        assert_eq!(file.path, manifest.root_dir().join(".skyzen/gen/.dev.vars"));
        let FileContents::Secret(contents) = &file.contents else {
            panic!("a file of values is not public: {:?}", file.contents);
        };
        assert_eq!(environment::expose(contents), "STRIPE_KEY='sk_live_123'\n");
    }

    #[test]
    fn a_named_environment_writes_the_file_wrangler_actually_loads() {
        // wrangler loads `.dev.vars.<env>` *instead of* `.dev.vars` when it exists, so writing the
        // plain name under `--env staging` would be read by nothing.
        let (_dir, manifest) = project(CLASSIC, "STRIPE_KEY=sk_live_123\n");
        let planned = work(
            &Action::Dev {
                runner_args: Vec::new(),
            },
            &manifest,
            Some("staging"),
            false,
        )
        .expect("plan");

        assert_eq!(
            planned.generated_files[0].path,
            manifest.root_dir().join(".skyzen/gen/.dev.vars.staging")
        );
    }

    #[test]
    fn dev_seeds_a_store_backed_secret_before_wrangler_starts() {
        let (_dir, manifest) = project(STORE_BACKED, "STRIPE_KEY=sk_live_123\n");
        let planned = work(
            &Action::Dev {
                runner_args: Vec::new(),
            },
            &manifest,
            None,
            false,
        )
        .expect("plan");

        // Nothing classic is left, so there is no file to write.
        assert!(planned.generated_files.is_empty(), "{planned:?}");
        let described: Vec<String> = planned.steps.iter().map(Step::describe).collect();
        assert_eq!(described.len(), 2, "{described:?}");
        assert!(described[0].contains("wrangler's local"), "{described:?}");
        assert!(described[0].contains("stripe-key"), "{described:?}");
        assert!(!described[0].contains("sk_live_123"), "{described:?}");
        // The supervised process is last, so the store is seeded by the time it reads it.
        assert!(
            described[1].contains("wrangler dev --local"),
            "{described:?}"
        );
    }

    #[test]
    fn setting_a_classic_secret_hands_wrangler_the_value_on_standard_input() {
        let (_dir, manifest) = project(CLASSIC, "");
        let planned = work(
            &Action::Secret(SecretAction::Set {
                name: VarName::try_from("STRIPE_KEY".to_owned()).expect("a name"),
                value: SecretString::from("sk_live_123"),
            }),
            &manifest,
            None,
            false,
        )
        .expect("plan");

        let commands = commands(&planned);
        assert_eq!(commands.len(), 1);
        assert!(commands[0]
            .display()
            .contains("wrangler secret put STRIPE_KEY"));
        assert!(!commands[0].display().contains("sk_live_123"));
        assert_eq!(stdin(commands[0]), Some("sk_live_123"));
    }

    #[test]
    fn setting_a_store_backed_secret_writes_the_account_store_rather_than_the_worker() {
        let (_dir, manifest) = project(STORE_BACKED, "");
        let planned = work(
            &Action::Secret(SecretAction::Set {
                name: VarName::try_from("STRIPE_KEY".to_owned()).expect("a name"),
                value: SecretString::from("sk_live_123"),
            }),
            &manifest,
            None,
            false,
        )
        .expect("plan");

        assert!(commands(&planned).is_empty(), "the id is not known yet");
        let described = planned.steps[0].describe();
        assert!(described.contains("the account's"), "{described}");
        assert!(described.contains("store_1"), "{described}");
        assert!(!described.contains("sk_live_123"), "{described}");
    }

    #[test]
    fn push_delivers_every_classic_secret_without_rebuilding() {
        let (_dir, manifest) = project(CLASSIC, "STRIPE_KEY=sk_live_123\n");
        let planned =
            work(&Action::Secret(SecretAction::Push), &manifest, None, false).expect("plan");

        let commands = commands(&planned);
        assert_eq!(commands.len(), 1);
        assert!(commands[0].display().contains("wrangler secret bulk"));
        assert_eq!(stdin(commands[0]), Some(r#"{"STRIPE_KEY":"sk_live_123"}"#));
    }

    #[test]
    fn push_with_only_store_backed_secrets_says_what_writes_one_instead() {
        let (_dir, manifest) = project(STORE_BACKED, "");
        let error = work(&Action::Secret(SecretAction::Push), &manifest, None, false)
            .expect_err("there is nothing a bulk upload could deliver");
        assert!(
            format!("{error:#}").contains("skyzen secret set"),
            "{error:#}"
        );
    }
}
