//! Provider dispatch: turning a command plus a manifest into something to run.
//!
//! Each provider builds a [`ProviderPlan`]; `main` executes it. Adding a provider means adding a
//! module and one arm here — nothing else in the CLI knows how many there are.

pub mod cloudflare;
mod native;

use crate::{
    capabilities,
    cli::{Provider, SecretCommand},
    environment, output,
    project::{Project, WASM_TARGET},
};
use anyhow::{Context, Result};
use cloudflare::build::BuildPlan;
use skyzen_manifest::Manifest;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// What the user asked the CLI to do, once `new`, `add` and `completions` — which need no
/// provider — have been handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Produce deployment artifacts and stop.
    Build {
        /// Build optimized artifacts.
        release: bool,
    },
    /// Run locally, rebuilding on change.
    Dev,
    /// Build and upload.
    Deploy,
    /// Stream logs from the deployed application.
    Logs {
        /// Arguments forwarded verbatim to the provider's log command.
        wrangler_args: Vec<String>,
    },
    /// Manage the deployed application's secrets.
    Secret(SecretCommand),
}

impl Action {
    /// The provider to use when the user named none.
    const fn default_provider(&self) -> Provider {
        match self {
            // Local development defaults to the native binary: it is the fast loop, and it needs
            // no cloud account.
            Self::Dev => Provider::Native,
            _ => Provider::Cloudflare,
        }
    }

    /// Whether the action is meaningless without a cloud provider.
    const fn requires_cloud(&self) -> bool {
        matches!(self, Self::Deploy | Self::Logs { .. } | Self::Secret(_))
    }
}

/// One external process to run.
#[derive(Debug, Clone)]
pub struct CommandPlan {
    /// The program to launch.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The directory to launch it in.
    pub cwd: Option<PathBuf>,
}

impl CommandPlan {
    /// A copy-pasteable rendering, for progress output and `--dry-run`.
    pub fn display(&self) -> String {
        let command = if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        };
        match &self.cwd {
            Some(cwd) => format!("(cd {} && {command})", cwd.display()),
            None => command,
        }
    }
}

/// A file the CLI generates before running anything.
#[derive(Debug, Clone)]
pub struct GeneratedFile {
    /// Where it goes.
    pub path: PathBuf,
    /// What goes in it.
    pub contents: String,
}

/// How the supervised process and the build relate while `dev` is running.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunMode {
    /// Run each command to completion, in order.
    #[default]
    Once,
    /// Supervise the command, restarting it on every debounced source change.
    Restart,
    /// Supervise the command and re-run the build on every debounced source change, leaving the
    /// command itself running.
    Rebuild,
}

/// What one provider decided to do.
#[derive(Debug, Clone, Default)]
pub struct ProviderPlan {
    /// External processes to run, in order.
    pub commands: Vec<CommandPlan>,
    /// Files to write first.
    pub generated_files: Vec<GeneratedFile>,
    /// The artifact build to perform, when the action needs artifacts.
    pub build: Option<BuildPlan>,
    /// How to supervise the commands.
    pub run_mode: RunMode,
    /// Environment to hand the supervised process, never applied to the CLI's own environment.
    pub child_env: Vec<(String, String)>,
    /// The directory to watch, when supervising.
    pub watch_root: Option<PathBuf>,
    /// Whether `--dry-run` should still execute this plan.
    ///
    /// `deploy --dry-run` maps onto `wrangler deploy --dry-run`, which validates the real bundle,
    /// so its plan runs for real and only the upload is skipped. Everything else prints instead.
    pub execute_despite_dry_run: bool,
}

/// Build the plan for one action.
///
/// # Errors
///
/// Fails when the manifest cannot be loaded, when the project is missing a crate its manifest
/// declarations need, or when the provider cannot prepare the action.
pub fn prepare(
    action: &Action,
    manifest_path: &Path,
    provider: Option<Provider>,
    environment: Option<&str>,
    dry_run: bool,
) -> Result<ProviderPlan> {
    let provider = provider.unwrap_or_else(|| action.default_provider());
    if provider == Provider::Native && action.requires_cloud() {
        anyhow::bail!(
            "`skyzen {}` needs a cloud provider; pass --provider cloudflare",
            action_name(action)
        );
    }

    let manifest = Manifest::load(manifest_path)?;
    let project = Project::load(manifest.root_dir())?;
    capabilities::ensure_present(manifest.data(), &project)?;

    match provider {
        Provider::Native => native::prepare(action, &manifest),
        Provider::Cloudflare => {
            cloudflare::prepare(action, &manifest, &project, environment, dry_run)
        }
    }
}

/// Run a provisioning pass, which owns its own manifest write-back rather than producing a plan.
///
/// # Errors
///
/// Fails when the manifest cannot be loaded or a resource cannot be created.
pub fn provision(
    manifest_path: &Path,
    provider: Option<Provider>,
    environment: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    match provider.unwrap_or(Provider::Cloudflare) {
        Provider::Native => {
            anyhow::bail!("`skyzen provision` needs a cloud provider; pass --provider cloudflare")
        }
        Provider::Cloudflare => {
            let manifest = Manifest::load(manifest_path)?;
            let config = manifest
                .cloudflare(environment)?
                .ok_or_else(|| anyhow::anyhow!("missing [cloudflare] section in Skyzen.toml"))?;
            let tasks = cloudflare::provision::plan(config);
            cloudflare::provision::run(&tasks, manifest.path(), environment, dry_run)
        }
    }
}

const fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Build { .. } => "build",
        Action::Dev => "dev",
        Action::Deploy => "deploy",
        Action::Logs { .. } => "logs",
        Action::Secret(_) => "secret",
    }
}

/// Run every doctor check for the selected providers.
///
/// Every check runs even after one fails: a first run usually has more than one thing wrong, and
/// reporting them one command at a time turns setup into a guessing game.
///
/// # Errors
///
/// Fails when any check failed, after all of them have been reported.
pub fn doctor(
    manifest_path: &Path,
    provider: Option<Provider>,
    environment: Option<&str>,
) -> Result<()> {
    let providers =
        provider.map_or_else(|| vec![Provider::Native, Provider::Cloudflare], |p| vec![p]);
    let mut failures = 0_usize;

    for provider in &providers {
        for binary in required_binaries(*provider) {
            if binary_exists(binary) {
                output::ok(format!("{}: found `{binary}`", provider.as_str()));
            } else {
                output::failed(format!("{}: `{binary}` not found", provider.as_str()));
                failures += 1;
            }
        }
    }

    if providers.contains(&Provider::Cloudflare) {
        failures += check_wasm_target();
        failures += check_wrangler_auth();
    }

    failures += check_manifest(manifest_path, &providers, environment);

    if failures == 0 {
        Ok(())
    } else {
        anyhow::bail!("{failures} check(s) failed; see the report above")
    }
}

const fn required_binaries(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Native => &["cargo"],
        Provider::Cloudflare => &["cargo", "wrangler"],
    }
}

/// The wasm target the Cloudflare build hard-requires — and the most common first-run failure.
fn check_wasm_target() -> usize {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(output) if output.status.success() => {
            if String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == WASM_TARGET)
            {
                output::ok(format!("cloudflare: `{WASM_TARGET}` target installed"));
                0
            } else {
                output::failed(format!(
                    "cloudflare: `{WASM_TARGET}` target is not installed; run `rustup target add {WASM_TARGET}`"
                ));
                1
            }
        }
        _ => {
            // A rustup-less toolchain (a distribution package, a container image) can still have
            // the target; there is nothing to check against, so this is not a failure.
            output::warn(format!(
                "could not run `rustup target list --installed`; make sure `{WASM_TARGET}` is available"
            ));
            0
        }
    }
}

fn check_wrangler_auth() -> usize {
    let output = Command::new("wrangler")
        .arg("whoami")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .status();

    match output {
        Ok(status) if status.success() => {
            output::ok("cloudflare: wrangler is authenticated");
            0
        }
        Ok(_) => {
            output::failed(
                "cloudflare: `wrangler whoami` failed; run `wrangler login` or set CLOUDFLARE_API_TOKEN",
            );
            1
        }
        // The missing-binary case is already reported by the binary check.
        Err(_) => 0,
    }
}

/// Parse the manifest, cross-check its bindings, and compare wasm-bindgen versions.
fn check_manifest(
    manifest_path: &Path,
    providers: &[Provider],
    environment: Option<&str>,
) -> usize {
    if !manifest_path.exists() {
        output::ok(format!(
            "no {} to check; services can be wired in Rust instead",
            manifest_path.display()
        ));
        return 0;
    }

    let manifest = match Manifest::load(manifest_path) {
        Ok(manifest) => {
            output::ok(format!("{} parses", manifest_path.display()));
            manifest
        }
        Err(error) => {
            output::failed(format!("{error}"));
            return 1;
        }
    };

    let mut failures = 0;
    if providers.contains(&Provider::Cloudflare) {
        failures += check_cloudflare_manifest(&manifest, environment);
    }
    failures
}

fn check_cloudflare_manifest(manifest: &Manifest, environment: Option<&str>) -> usize {
    let mut failures = 0;

    let config = match manifest.cloudflare(environment) {
        Ok(Some(config)) => Some(config),
        Ok(None) => {
            output::warn("Skyzen.toml has no [cloudflare] section; `skyzen deploy` needs one");
            None
        }
        Err(error) => {
            output::failed(format!("{error}"));
            failures += 1;
            None
        }
    };

    if let Some(config) = config {
        let problems = cloudflare::binding_problems(manifest.data(), config);
        if problems.is_empty() {
            output::ok("cloudflare: every portable capability has a matching binding");
        } else {
            for problem in &problems {
                output::failed(format!("cloudflare: {problem}"));
            }
            failures += problems.len();
        }
    }

    match Project::load(manifest.root_dir())
        .and_then(|project| cloudflare::build::check_wasm_bindgen_agreement(&project))
    {
        Ok(()) => output::ok(format!(
            "cloudflare: wasm-bindgen agrees with the embedded {} generator",
            cloudflare::build::embedded_wasm_bindgen_version()
        )),
        Err(error) => {
            output::failed(format!("cloudflare: {error:#}"));
            failures += 1;
        }
    }

    failures
}

fn binary_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Load the project's `.env` files and check every variable the manifest declares is available.
///
/// # Errors
///
/// Fails when a dotenv file cannot be read, or when a declared variable is set nowhere.
pub fn prepare_child_environment(manifest: &Manifest) -> Result<Vec<(String, String)>> {
    let dotenv = environment::load_dotenv_files(manifest.root_dir())
        .context("failed to load the project's .env files")?;
    environment::ensure_available(&environment::required_variables(manifest.data()), &dotenv)?;
    Ok(dotenv.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::{Action, CommandPlan};
    use crate::cli::Provider;
    use std::path::PathBuf;

    #[test]
    fn dev_defaults_to_native_and_deployment_defaults_to_the_cloud() {
        assert_eq!(Action::Dev.default_provider(), Provider::Native);
        assert_eq!(Action::Deploy.default_provider(), Provider::Cloudflare);
        assert_eq!(
            Action::Build { release: true }.default_provider(),
            Provider::Cloudflare
        );
    }

    #[test]
    fn the_cloud_only_actions_are_the_ones_that_talk_to_an_account() {
        assert!(Action::Deploy.requires_cloud());
        assert!(Action::Logs {
            wrangler_args: Vec::new()
        }
        .requires_cloud());
        assert!(!Action::Dev.requires_cloud());
        assert!(!Action::Build { release: false }.requires_cloud());
    }

    #[test]
    fn a_command_renders_with_its_working_directory() {
        let plan = CommandPlan {
            program: "wrangler".to_owned(),
            args: vec!["dev".to_owned()],
            cwd: Some(PathBuf::from("/tmp/app")),
        };
        assert_eq!(plan.display(), "(cd /tmp/app && wrangler dev)");
    }
}
