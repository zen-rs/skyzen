//! The Azure Functions provider.
//!
//! A Skyzen application deploys to Functions as a [custom handler]: the Function App runs the
//! compiled binary as a web server and forwards every event to it. What the platform needs is not
//! the binary but a *bundle* — `host.json` saying how to start it, one `function.json` per function
//! — and this module generates that bundle into `.skyzen/gen/azure/`, stages the binary beside it,
//! and hands the directory to `func azure functionapp publish`.
//!
//! [custom handler]: https://learn.microsoft.com/azure/azure-functions/functions-custom-handlers

mod app_settings;
mod bundle;

use crate::{
    environment::{self, VariableKind},
    output,
    project::Project,
    providers::{
        prepare_child_environment, secrets::Delivery, Action, CommandPlan, CommandStdin,
        ProviderPlan, RunMode, SecretAction, Step, Task,
    },
};
use anyhow::{Context, Result};
use app_settings::{AppSettingNames, AppSettings, FunctionApp};
use skyzen_manifest::{AzureSection, Manifest};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Where the generated Functions bundle lives, relative to the project root.
///
/// A directory of its own because `func` publishes *everything* in the app root: putting the
/// bundle at the project root would upload the whole repository.
const BUNDLE_DIR: &str = ".skyzen/gen/azure";

/// The Linux target a Function App can actually run, and the one `rustup target add` is enough for.
pub const DEFAULT_LINUX_TARGET: &str = "x86_64-unknown-linux-musl";

/// Build the plan for one Azure action.
///
/// # Errors
///
/// Fails when the project cannot name the binary to publish, when `[azure]` does not say which
/// Function App to act on, or when the action has no Azure meaning.
pub fn prepare(action: &Action, manifest: &Manifest, project: &Project) -> Result<ProviderPlan> {
    let config = manifest.data().azure.clone().unwrap_or_default();
    let root_dir = manifest.root_dir().to_path_buf();
    let bundle_dir = root_dir.join(BUNDLE_DIR);

    // Before the binary is looked up: a secret action neither builds nor publishes anything, so a
    // project that cannot name one is no obstacle to setting a value.
    if let Action::Secret(secret) = action {
        return Ok(ProviderPlan {
            steps: vec![secret_step(secret, manifest, &config)?],
            ..ProviderPlan::default()
        });
    }

    let binary = project.binary_target_name()?.to_owned();

    let needs_bundle = matches!(action, Action::Build { .. } | Action::Deploy);
    if !needs_bundle {
        return Ok(ProviderPlan {
            steps: vec![Step::Command(non_bundling_command(
                action, &config, &root_dir,
            )?)],
            ..ProviderPlan::default()
        });
    }

    let files = bundle::render(&config, &binary, &bundle_dir)?;
    let build = HandlerBuild {
        root_dir,
        target: config.target.clone(),
        binary: binary.clone(),
        target_directory: project.target_directory().to_path_buf(),
        staged_path: bundle_dir.join(&binary),
        // A `build` is for looking at what would be published; only a deploy has to run there.
        require_linux: matches!(action, Action::Deploy),
    };

    let mut child_env = Vec::new();
    let steps = match action {
        Action::Build { .. } => Vec::new(),
        Action::Deploy => {
            // Both are read before anything is built: a deploy that would have nowhere to deliver
            // its variables, or no value for one of them, must fail before it uploads a binary.
            let app = function_app(&config)?;
            let publish = publish_command(&config, &bundle_dir)?;
            let prepared = prepare_child_environment(manifest, VariableKind::ALL)?;
            child_env = prepared.child_env;
            let mut steps = vec![Step::Command(publish)];
            let delivery = Delivery::from_resolved(&prepared.resolved);
            if !delivery.is_empty() {
                steps.push(Step::Task(Box::new(AppSettings::new(app, delivery))));
            }
            steps
        }
        other => unreachable!("{} does not need a bundle", super::action_name(other)),
    };

    Ok(ProviderPlan {
        steps,
        generated_files: files,
        build: Some(Box::new(build)),
        run_mode: RunMode::Once,
        child_env,
        watch_root: None,
        execute_despite_dry_run: false,
    })
}

/// The one step a `skyzen secret` action performs.
///
/// Functions has no secret store of its own: an app's settings *are* its environment, so every one
/// of these is the same read-modify-write against them.
///
/// # Errors
///
/// Fails when `[azure]` does not say which app to talk to, when a declared variable is set
/// nowhere, or when there is nothing at all to push.
fn secret_step(action: &SecretAction, manifest: &Manifest, config: &AzureSection) -> Result<Step> {
    let app = function_app(config)?;
    let delivery = match action {
        SecretAction::Set { name, value } => Delivery::one(name.as_str(), value),
        SecretAction::Push => {
            let resolved = prepare_child_environment(manifest, VariableKind::ALL)?.resolved;
            let delivery = Delivery::from_resolved(&resolved);
            if delivery.is_empty() {
                anyhow::bail!(
                    "there is nothing to push: Skyzen.toml declares no [[secret]] and no native \
                     wiring variable"
                );
            }
            delivery
        }
        SecretAction::List => return Ok(Step::Task(Box::new(AppSettingNames::new(app)))),
    };
    Ok(Step::Task(Box::new(AppSettings::new(app, delivery))))
}

/// The one command an action that needs no bundle runs.
fn non_bundling_command(
    action: &Action,
    config: &AzureSection,
    root_dir: &Path,
) -> Result<CommandPlan> {
    match action {
        Action::Logs { wrangler_args } => {
            let mut args = vec![
                "azure".to_owned(),
                "functionapp".to_owned(),
                "logstream".to_owned(),
                app_name(config)?,
            ];
            args.extend(wrangler_args.iter().cloned());
            Ok(CommandPlan {
                program: "func".to_owned(),
                args,
                cwd: Some(root_dir.to_path_buf()),
                stdin: CommandStdin::Inherit,
            })
        }
        other => anyhow::bail!(
            "`skyzen {}` has no Azure implementation{}",
            super::action_name(other),
            unsupported_hint(other)
        ),
    }
}

/// What to suggest when an action has no Azure counterpart.
const fn unsupported_hint(action: &Action) -> &'static str {
    match action {
        Action::Dev { .. } => {
            ": run it as an ordinary server with `skyzen dev`, or start the host over a built \
             bundle with `func start` from .skyzen/gen/azure"
        }
        Action::Migrate { .. } => {
            ": point `skyzen migrate` at the database directly rather than through the Function App"
        }
        _ => "",
    }
}

/// `func azure functionapp publish`, run from the generated bundle.
fn publish_command(config: &AzureSection, bundle_dir: &Path) -> Result<CommandPlan> {
    Ok(CommandPlan {
        program: "func".to_owned(),
        args: vec![
            "azure".to_owned(),
            "functionapp".to_owned(),
            "publish".to_owned(),
            app_name(config)?,
        ],
        cwd: Some(bundle_dir.to_path_buf()),
        stdin: CommandStdin::Inherit,
    })
}

/// The Function App to talk to, which nothing can be inferred from.
fn app_name(config: &AzureSection) -> Result<String> {
    config.app_name.clone().context(
        "missing `app_name` in [azure]; it names the Function App to publish to, and there is \
         nothing to infer it from",
    )
}

/// The Function App an ARM call addresses.
///
/// `func azure functionapp publish` finds an app by name alone, but ARM addresses the settings
/// resource by its full id, so a deploy — which delivers the runtime variables — and every
/// `skyzen secret` action need all three parts. Reported together with the same wording as
/// `app_name`, because a project missing one is usually missing both.
fn function_app(config: &AzureSection) -> Result<FunctionApp> {
    Ok(FunctionApp {
        name: app_name(config)?,
        subscription_id: config.subscription_id.clone().context(
            "missing `subscription_id` in [azure]; it is half of the ARM id of the application \
             settings a deployment delivers its runtime variables through, and there is nothing \
             to infer it from (`az account show --query id -o tsv`)",
        )?,
        resource_group: config.resource_group.clone().context(
            "missing `resource_group` in [azure]; it is the other half of that ARM id (`az \
             functionapp list --query \"[].{name:name,group:resourceGroup}\" -o table`)",
        )?,
    })
}

/// Compile the handler and stage it inside the bundle.
///
/// Staging is what makes this a build rather than a command: `func` uploads the bundle directory
/// and nothing outside it, so the binary has to be *in* there, next to the `host.json` that names
/// it.
#[derive(Debug, Clone)]
struct HandlerBuild {
    /// The project root, which cargo runs in.
    root_dir: PathBuf,
    /// The target triple to cross-compile to, when the manifest names one.
    target: Option<String>,
    /// The binary's name, which is also its name inside the bundle.
    binary: String,
    /// Cargo's build directory, where the compiled binary lands.
    target_directory: PathBuf,
    /// Where the binary is copied to.
    staged_path: PathBuf,
    /// Whether a non-Linux binary is a failure rather than a warning.
    require_linux: bool,
}

impl HandlerBuild {
    /// Where cargo leaves the release binary.
    fn artifact_path(&self) -> PathBuf {
        let mut path = self.target_directory.clone();
        if let Some(target) = &self.target {
            path.push(target);
        }
        path.push("release");
        path.push(&self.binary);
        path
    }

    /// The cargo invocation, as a plan so `--dry-run` can print it verbatim.
    fn command(&self) -> CommandPlan {
        let mut args = vec!["build".to_owned(), "--release".to_owned()];
        if let Some(target) = &self.target {
            args.push("--target".to_owned());
            args.push(target.clone());
        }
        CommandPlan {
            program: "cargo".to_owned(),
            args,
            cwd: Some(self.root_dir.clone()),
            stdin: CommandStdin::Inherit,
        }
    }
}

impl Task for HandlerBuild {
    fn describe(&self) -> String {
        format!(
            "{} && stage {} into {}",
            self.command().display(),
            self.binary,
            self.staged_path
                .parent()
                .unwrap_or(&self.staged_path)
                .display()
        )
    }

    fn run(&self) -> Result<()> {
        let command = self.command();
        output::step(command.display());
        let status = Command::new(&command.program)
            .args(&command.args)
            .current_dir(&self.root_dir)
            .status()
            .context("failed to launch cargo")?;
        if !status.success() {
            anyhow::bail!("cargo build failed with status {status}");
        }

        let artifact = self.artifact_path();
        if !artifact.exists() {
            anyhow::bail!(
                "the build produced no binary at {}; does the package define a [[bin]] target?",
                artifact.display()
            );
        }
        ensure_runs_on_linux(&artifact, self.target.as_deref(), self.require_linux)?;

        if let Some(parent) = self.staged_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(&artifact, &self.staged_path).with_context(|| {
            format!(
                "failed to stage {} into {}",
                artifact.display(),
                self.staged_path.display()
            )
        })?;
        output::step(format!("staged {}", self.staged_path.display()));
        Ok(())
    }
}

/// The first bytes of an ELF file, which is what a Function App can execute.
const ELF_MAGIC: &[u8] = b"\x7fELF";

/// Refuse to publish a binary the Function App cannot run.
///
/// A Function App runs Linux, and `func azure functionapp publish` uploads whatever it is given
/// without looking — so a handler built on macOS publishes successfully and then fails to start,
/// with nothing in the portal to say why. Reading the file's magic number is the cheapest way to
/// know before the upload.
fn ensure_runs_on_linux(artifact: &Path, target: Option<&str>, require: bool) -> Result<()> {
    let header = fs::read(artifact)
        .with_context(|| format!("failed to read {}", artifact.display()))?
        .into_iter()
        .take(ELF_MAGIC.len())
        .collect::<Vec<_>>();
    if header == ELF_MAGIC {
        return Ok(());
    }

    let remedy = format!(
        "build for Linux instead: add `target = \"{DEFAULT_LINUX_TARGET}\"` to [azure], run \
         `rustup target add {DEFAULT_LINUX_TARGET}`, and install a linker for it (`brew install \
         filosottile/musl-cross/musl-cross` on macOS, with `[target.{DEFAULT_LINUX_TARGET}] \
         linker = \"x86_64-linux-musl-gcc\"` in .cargo/config.toml)"
    );

    if require {
        anyhow::bail!(
            "{} is not a Linux executable{}, so the Function App could not run it; {remedy}",
            artifact.display(),
            target.map_or_else(String::new, |target| format!(" (built for {target})"))
        );
    }
    output::warn(format!(
        "{} is not a Linux executable, so it is not publishable as it is; {remedy}",
        artifact.display()
    ));
    Ok(())
}

/// Report whether the Linux target an Azure deployment needs is installed.
///
/// Only meaningful when the manifest names one: without `target`, the build is whatever the host
/// is, which is right on a Linux CI machine and caught before the upload anywhere else.
pub fn check_linux_target(manifest_path: &Path) -> usize {
    let target = environment::load_manifest(manifest_path)
        .ok()
        .and_then(|manifest| manifest.data().azure.as_ref()?.target.clone());
    let Some(target) = target else {
        output::warn(
            "azure: [azure] names no `target`; a handler built on macOS or Windows cannot run on a \
             Function App",
        );
        return 0;
    };

    let installed = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();

    match installed {
        Ok(output) if output.status.success() => {
            if String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == target)
            {
                output::ok(format!("azure: `{target}` target installed"));
                0
            } else {
                output::failed(format!(
                    "azure: `{target}` is not installed; run `rustup target add {target}`"
                ));
                1
            }
        }
        // A rustup-less toolchain can still have the target; there is nothing to check against.
        _ => {
            output::warn(format!(
                "could not run `rustup target list --installed`; make sure `{target}` is available"
            ));
            0
        }
    }
}

/// Report what `skyzen doctor --provider azure` can tell from the manifest alone.
pub fn check_manifest(manifest: &Manifest) -> usize {
    // The Functions host runs the native binary, so a deploy delivers both kinds.
    super::report_runtime_variables(manifest, VariableKind::ALL, "azure", "skyzen deploy");

    let Some(config) = manifest.data().azure.as_ref() else {
        output::failed(
            "azure: Skyzen.toml has no [azure] section; `skyzen deploy` needs `app_name`",
        );
        return 1;
    };

    let mut failures = 0;
    match function_app(config) {
        Ok(app) => output::ok(format!(
            "azure: publishing to {} in {}/{}",
            app.name, app.subscription_id, app.resource_group
        )),
        Err(error) => {
            output::failed(format!("azure: {error}"));
            failures += 1;
        }
    }

    for trigger in &config.queue_triggers {
        output::ok(format!(
            "azure: queue trigger `{}` reads `{}` through {}",
            trigger.function, trigger.queue, trigger.connection_env
        ));
    }

    failures
}

#[cfg(test)]
mod tests {
    use super::{ensure_runs_on_linux, prepare, ELF_MAGIC};
    use crate::providers::{Action, SecretAction, Step};
    use secrecy::SecretString;
    use skyzen_manifest::{Manifest, VarName};

    /// An `[azure]` section naming everything a deploy needs.
    const ADDRESSED: &str = "[azure]\napp_name = \"skyzen-demo\"\n\
         subscription_id = \"00000000-0000-0000-0000-000000000000\"\n\
         resource_group = \"skyzen-rg\"\n";

    fn manifest(source: &str) -> Manifest {
        Manifest::parse(source, "Skyzen.toml", "/tmp/app").expect("valid manifest")
    }

    fn project() -> crate::project::Project {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin-app");
        crate::project::Project::load(&dir).expect("the fixture package loads")
    }

    #[test]
    fn a_deploy_publishes_the_generated_bundle_and_builds_the_handler_first() {
        let plan = prepare(
            &Action::Deploy,
            &manifest(&format!(
                "{ADDRESSED}target = \"x86_64-unknown-linux-musl\"\n"
            )),
            &project(),
        )
        .expect("plan");

        let build = plan.build.as_ref().expect("a deploy builds the handler");
        let described = build.describe();
        assert!(
            described.contains("cargo build --release --target x86_64-unknown-linux-musl"),
            "{described}"
        );
        assert!(described.contains("stage demo"), "{described}");

        let commands: Vec<String> = plan.steps.iter().map(Step::describe).collect();
        assert_eq!(commands.len(), 1);
        assert!(
            commands[0].contains("func azure functionapp publish skyzen-demo"),
            "{commands:?}"
        );
        // Published from the bundle, because `func` uploads the directory it is run in.
        assert!(commands[0].contains(".skyzen/gen/azure"), "{commands:?}");

        // host.json plus one function.json for the catch-all HTTP function.
        assert_eq!(plan.generated_files.len(), 3, "{:?}", plan.generated_files);
    }

    #[test]
    fn a_build_generates_the_bundle_without_publishing_anything() {
        let plan = prepare(
            &Action::Build { release: true },
            &manifest("[azure]\napp_name = \"skyzen-demo\"\n"),
            &project(),
        )
        .expect("a build needs no ARM id: it uploads nothing");

        assert!(plan.steps.is_empty(), "a build uploads nothing");
        assert!(plan.build.is_some());
    }

    #[test]
    fn a_deploy_without_an_app_name_says_so_before_building_anything() {
        let error = prepare(&Action::Deploy, &manifest("[azure]\n"), &project())
            .expect_err("there is nothing to publish to");

        assert!(error.to_string().contains("app_name"), "{error}");
    }

    #[test]
    fn a_deploy_that_cannot_address_the_settings_resource_names_the_missing_key() {
        for (source, missing) in [
            (
                "[azure]\napp_name = \"skyzen-demo\"\nresource_group = \"skyzen-rg\"\n",
                "subscription_id",
            ),
            (
                "[azure]\napp_name = \"skyzen-demo\"\n\
                 subscription_id = \"00000000-0000-0000-0000-000000000000\"\n",
                "resource_group",
            ),
        ] {
            let error = prepare(&Action::Deploy, &manifest(source), &project())
                .expect_err("the runtime variables would have nowhere to go");
            assert!(error.to_string().contains(missing), "{error}");

            let error = prepare(
                &Action::Secret(SecretAction::List),
                &manifest(source),
                &project(),
            )
            .expect_err("nor can a secret action address the app");
            assert!(error.to_string().contains(missing), "{error}");
        }
    }

    #[test]
    fn a_deploy_ends_by_delivering_the_variables_it_names_and_no_values() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(".env"), "STRIPE_KEY=sk_live_123\n").expect("write .env");
        let manifest = Manifest::parse(
            &format!("[[secret]]\nname = \"STRIPE_KEY\"\n\n{ADDRESSED}"),
            dir.path().join("Skyzen.toml"),
            dir.path(),
        )
        .expect("valid manifest");

        let plan = prepare(&Action::Deploy, &manifest, &project()).expect("plan");
        let last = plan.steps.last().expect("a deploy delivers its settings");
        let described = last.describe();

        assert!(matches!(last, Step::Task(_)), "{described}");
        assert!(described.contains("skyzen-demo"), "{described}");
        assert!(described.contains("STRIPE_KEY"), "{described}");
        assert!(!described.contains("sk_live_123"), "{described}");
        // The publish still comes first: settings that named a binary nobody uploaded would be
        // delivered to the previous deployment.
        assert!(
            plan.steps[0].describe().contains("functionapp publish"),
            "{:?}",
            plan.steps[0]
        );
    }

    #[test]
    fn a_deploy_refuses_when_a_declared_variable_is_set_nowhere() {
        let error = prepare(
            &Action::Deploy,
            &manifest(&format!(
                "[[secret]]\nname = \"SKYZEN_TEST_AZURE_UNSET_SECRET\"\n\n{ADDRESSED}"
            )),
            &project(),
        )
        .expect_err("the published handler would panic at startup");

        assert!(
            format!("{error:#}").contains("SKYZEN_TEST_AZURE_UNSET_SECRET"),
            "{error}"
        );
    }

    #[test]
    fn setting_one_secret_delivers_that_pair_and_names_no_value() {
        let plan = prepare(
            &Action::Secret(SecretAction::Set {
                name: VarName::try_from("STRIPE_KEY".to_owned()).expect("a name"),
                value: SecretString::from("sk_live_123"),
            }),
            &manifest(&format!("[[secret]]\nname = \"STRIPE_KEY\"\n\n{ADDRESSED}")),
            &project(),
        )
        .expect("plan");

        let described = plan.steps[0].describe();
        assert!(described.contains("STRIPE_KEY"), "{described}");
        assert!(described.contains("skyzen-demo"), "{described}");
        assert!(!described.contains("sk_live_123"), "{described}");
        // A secret action publishes nothing, so it generates no bundle either.
        assert!(plan.generated_files.is_empty());
        assert!(plan.build.is_none());
    }

    #[test]
    fn logs_stream_from_the_named_function_app() {
        let plan = prepare(
            &Action::Logs {
                wrangler_args: Vec::new(),
            },
            &manifest("[azure]\napp_name = \"skyzen-demo\"\n"),
            &project(),
        )
        .expect("plan");

        assert!(
            plan.steps[0]
                .describe()
                .contains("func azure functionapp logstream skyzen-demo"),
            "{:?}",
            plan.steps[0]
        );
    }

    #[test]
    fn an_action_with_no_azure_meaning_says_what_to_do_instead() {
        let error = prepare(
            &Action::Dev {
                runner_args: Vec::new(),
            },
            &manifest("[azure]\napp_name = \"skyzen-demo\"\n"),
            &project(),
        )
        .expect_err("the Functions host is not a dev server");

        assert!(error.to_string().contains("func start"), "{error}");
    }

    #[test]
    fn a_non_linux_binary_is_refused_before_it_can_be_published() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mach_o = dir.path().join("demo");
        // A Mach-O header: what a macOS build actually produces.
        std::fs::write(&mach_o, [0xcf, 0xfa, 0xed, 0xfe, 0x0c]).expect("write");

        let error = ensure_runs_on_linux(&mach_o, None, true).expect_err("not publishable");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("not a Linux executable"), "{rendered}");
        assert!(
            rendered.contains("rustup target add x86_64-unknown-linux-musl"),
            "{rendered}"
        );
    }

    #[test]
    fn a_linux_binary_passes_the_same_check() {
        let dir = tempfile::tempdir().expect("temp dir");
        let elf = dir.path().join("demo");
        std::fs::write(&elf, [ELF_MAGIC, b"\x02\x01\x01"].concat()).expect("write");

        ensure_runs_on_linux(&elf, Some("x86_64-unknown-linux-musl"), true)
            .expect("an ELF binary is what a Function App runs");
    }

    #[test]
    fn a_build_only_warns_about_a_binary_the_host_could_not_run() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mach_o = dir.path().join("demo");
        std::fs::write(&mach_o, [0xcf, 0xfa, 0xed, 0xfe]).expect("write");

        // `skyzen build` is for looking at the bundle; only a deploy has to run on the host.
        ensure_runs_on_linux(&mach_o, None, false).expect("a build is not a publish");
    }
}
