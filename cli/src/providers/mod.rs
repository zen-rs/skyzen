//! Provider dispatch: turning a command plus a manifest into something to run.
//!
//! Each provider builds a [`ProviderPlan`]; `main` executes it. Adding a provider means adding a
//! module and one arm here — nothing else in the CLI knows how many there are.

pub mod aws;
pub mod azure;
pub mod cloudflare;
mod native;
pub mod secrets;

use crate::{
    capabilities,
    cli::Provider,
    environment::{self, ResolvedVariables, RuntimeVariable, VariableKind},
    output,
    project::{Project, WASM_TARGET},
};
use anyhow::{Context, Result};
use secrecy::{ExposeSecret as _, SecretString};
use skyzen_manifest::{Manifest, SkyzenManifest, VarName};
use std::{
    fmt::Debug,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

/// What the user asked the CLI to do, once `new`, `add` and `completions` — which need no
/// provider — have been handled.
#[derive(Debug)]
pub enum Action {
    /// Produce deployment artifacts and stop.
    Build {
        /// Build optimized artifacts.
        release: bool,
    },
    /// Run locally, rebuilding on change.
    Dev {
        /// Arguments forwarded verbatim to the underlying runner.
        runner_args: Vec<String>,
    },
    /// Build and upload.
    Deploy,
    /// Stream logs from the deployed application.
    Logs {
        /// Arguments forwarded verbatim to the provider's log command.
        wrangler_args: Vec<String>,
    },
    /// Manage the deployed application's secrets.
    Secret(SecretAction),
    /// Apply pending SQL migrations, or report which are still pending.
    Migrate {
        /// Act on the local emulator rather than the deployed database.
        local: bool,
        /// Report what is applied and what is pending instead of applying anything.
        status: bool,
    },
}

/// One `skyzen secret` operation, carrying the value the user piped in when there is one.
///
/// `main` reads the value from the CLI's own standard input before any provider is asked for a
/// plan: every provider then sees the same thing, and no provider has to know how a terminal
/// prompts.
#[derive(Debug)]
pub enum SecretAction {
    /// Set one secret's value.
    Set {
        /// The secret's name, checked against the manifest's `[[secret]]` entries before dispatch.
        name: VarName,
        /// The value to deliver.
        value: SecretString,
    },
    /// Deliver every declared secret's local value, without rebuilding anything.
    Push,
    /// List the secrets the deployment has.
    List,
}

impl Action {
    /// The provider to use when the user named none.
    const fn default_provider(&self) -> Provider {
        match self {
            // Local development defaults to the native binary: it is the fast loop, and it needs
            // no cloud account.
            Self::Dev { .. } => Provider::Native,
            _ => Provider::Cloudflare,
        }
    }

    /// Whether the action is meaningless without a cloud provider.
    const fn requires_cloud(&self) -> bool {
        matches!(
            self,
            Self::Deploy | Self::Logs { .. } | Self::Secret(_) | Self::Migrate { .. }
        )
    }
}

/// One external process to run.
#[derive(Debug)]
pub struct CommandPlan {
    /// The program to launch.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The directory to launch it in.
    pub cwd: Option<PathBuf>,
    /// What the process reads on its standard input.
    pub stdin: CommandStdin,
}

impl CommandPlan {
    /// A copy-pasteable rendering, for progress output and `--dry-run`.
    ///
    /// The standard input is deliberately not part of it: a value delivered there is the one thing
    /// about a command that must never reach a terminal or a log.
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

    /// The same command, with `value` written to its standard input.
    #[must_use]
    pub fn with_stdin(mut self, value: SecretString) -> Self {
        self.stdin = CommandStdin::Secret(value);
        self
    }
}

/// What an external process reads on its standard input.
///
/// The reason it is a field rather than a convention: a value handed to wrangler travels here and
/// nowhere else, so [`CommandPlan::display`] can print every part of a command it *does* carry.
#[derive(Debug, Default)]
pub enum CommandStdin {
    /// The CLI's own standard input, so a tool that prompts stays usable.
    #[default]
    Inherit,
    /// A value written to the process, after which the pipe is closed.
    Secret(SecretString),
}

impl CommandStdin {
    /// How the child's standard input is wired up.
    fn stdio(&self) -> Stdio {
        match self {
            Self::Inherit => Stdio::inherit(),
            Self::Secret(_) => Stdio::piped(),
        }
    }

    /// Write the value to a spawned child, closing the pipe afterwards.
    ///
    /// On a thread of its own, because a value larger than the pipe buffer would otherwise
    /// deadlock: the parent blocks writing while the child waits for input the parent is not
    /// getting round to sending. The value is copied into the thread as another [`SecretString`],
    /// so the copy is zeroized when the writer is done with it.
    fn write_to(&self, child: &mut Child) -> Result<()> {
        let Self::Secret(value) = self else {
            return Ok(());
        };
        let mut pipe = child
            .stdin
            .take()
            .context("the process was started without a standard input pipe")?;
        let value = environment::duplicate(value);
        std::thread::spawn(move || {
            if let Err(error) = pipe.write_all(value.expose_secret().as_bytes()) {
                output::warn(format!(
                    "failed to write a value to the process's standard input: {error}"
                ));
            }
        });
        Ok(())
    }
}

/// One piece of work in a provider's plan.
#[derive(Debug)]
pub enum Step {
    /// An external process to run.
    Command(CommandPlan),
    /// Work the CLI performs itself.
    Task(Box<dyn Task>),
}

impl Step {
    /// A one-line description, for progress output and `--dry-run`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Command(command) => command.display(),
            Self::Task(task) => task.describe(),
        }
    }

    /// Perform it.
    ///
    /// # Errors
    ///
    /// Fails when the process cannot be launched, when it exits non-zero, or when the task does.
    pub fn run(&self, child_env: &[(String, String)]) -> Result<()> {
        match self {
            Self::Command(command) => run_command(command, child_env),
            Self::Task(task) => {
                output::step(task.describe());
                task.run()
            }
        }
    }
}

/// Work the CLI does itself rather than by launching a process.
///
/// An artifact build is one — wasm-bindgen glue for Cloudflare, a Linux cross-compile staged into
/// a bundle for Azure — and so is anything whose next command depends on what the previous one
/// answered, such as seeding a Secrets Store entry whose id is only known after listing the store.
/// A step that is just a process uses a [`CommandPlan`] instead, where `--dry-run` prints it
/// verbatim.
pub trait Task: Debug + Send {
    /// A one-line description for progress output and `--dry-run`.
    fn describe(&self) -> String;

    /// Do the work.
    ///
    /// # Errors
    ///
    /// Fails when the compiler, a tool it drives, or the filesystem does.
    fn run(&self) -> Result<()>;
}

/// A file the CLI generates before running anything.
#[derive(Debug)]
pub struct GeneratedFile {
    /// Where it goes.
    pub path: PathBuf,
    /// What goes in it.
    pub contents: FileContents,
}

/// What a generated file holds, and therefore what `--dry-run` may print.
///
/// A generated `wrangler.toml` or `host.json` is configuration a user should be able to read
/// before it is written; a `.dev.vars` is values. Making the distinction a type rather than a
/// convention means a new generated file has to say which it is, and a printer cannot forget.
#[derive(Debug)]
pub enum FileContents {
    /// Configuration, printed verbatim by `--dry-run`.
    Public(String),
    /// Values, never printed and written with owner-only permissions.
    Secret(SecretString),
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
#[derive(Debug, Default)]
pub struct ProviderPlan {
    /// The work to perform after the build, in order.
    pub steps: Vec<Step>,
    /// Files to write first.
    pub generated_files: Vec<GeneratedFile>,
    /// The artifact build to perform, when the action needs artifacts.
    ///
    /// Separate from [`steps`](Self::steps) because it is the *supervised* build: `skyzen dev`
    /// re-runs this one on every source change, and nothing else.
    pub build: Option<Box<dyn Task>>,
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

impl ProviderPlan {
    /// The steps a supervised run performs first, and the process it then supervises.
    ///
    /// The supervised process is the last step, and everything before it is a preamble that has to
    /// have happened by the time it starts — seeding wrangler's local Secrets Store before
    /// `wrangler dev` reads it, for one.
    ///
    /// # Errors
    ///
    /// Fails when the plan ends in something other than a command, which would mean supervising
    /// nothing while silently skipping the work that was planned.
    pub fn supervised(&self) -> Result<(&[Step], &CommandPlan)> {
        let (last, preamble) = self
            .steps
            .split_last()
            .context("a supervised run needs a command to supervise")?;
        let Step::Command(command) = last else {
            anyhow::bail!(
                "a supervised run must end in the process to supervise, not `{}`",
                last.describe()
            );
        };
        Ok((preamble, command))
    }
}

/// Run every step in order, or report what they would have been.
///
/// # Errors
///
/// Fails on the first step that does, naming it.
pub fn run_steps(steps: &[Step], simulate: bool, child_env: &[(String, String)]) -> Result<()> {
    for step in steps {
        if simulate {
            output::dry_run(step.describe());
            continue;
        }
        step.run(child_env)?;
    }
    Ok(())
}

/// Run one external command to completion.
///
/// # Errors
///
/// Fails when the program cannot be launched or exits non-zero.
pub fn run_command(command: &CommandPlan, child_env: &[(String, String)]) -> Result<()> {
    let display = command.display();
    output::step(&display);
    let mut child = spawn_command(command, child_env)?;
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {}", command.program))?;
    if !status.success() {
        anyhow::bail!("command failed with status {status}: {display}");
    }
    Ok(())
}

/// Start one external command, handing it whatever its standard input carries.
///
/// # Errors
///
/// Fails when the program cannot be launched.
pub fn spawn_command(command: &CommandPlan, child_env: &[(String, String)]) -> Result<Child> {
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .envs(child_env.iter().map(|(key, value)| (key, value)))
        .stdin(command.stdin.stdio())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(cwd) = &command.cwd {
        process.current_dir(cwd);
    }

    let mut child = process
        .spawn()
        .with_context(|| format!("failed to launch {}", command.program))?;
    command.stdin.write_to(&mut child)?;
    Ok(child)
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

    let manifest = load_or_empty(manifest_path)?;
    if let Action::Secret(SecretAction::Set { name, .. }) = action {
        ensure_declared_secret(manifest.data(), name)?;
    }
    let project = Project::load(manifest.root_dir())?;
    capabilities::ensure_present(manifest.data(), &project)?;

    match provider {
        Provider::Native => native::prepare(action, &manifest),
        Provider::Cloudflare => {
            cloudflare::prepare(action, &manifest, &project, environment, dry_run)
        }
        Provider::Aws => aws::prepare(action, &manifest, &project),
        Provider::Azure => azure::prepare(action, &manifest, &project),
    }
}

/// Load the manifest, or synthesize an empty one when the project has none.
///
/// `Skyzen.toml` is optional — an application can wire every service by hand in Rust, and the
/// macros treat a missing file the same way — so `skyzen dev` has to work in a project that never
/// had one. An empty manifest declares no capabilities and no environment variables, which is
/// exactly right for the native path; the Cloudflare path then fails with "missing [cloudflare]
/// section", which says what to do, rather than with a file-not-found.
pub fn load_or_empty(manifest_path: &Path) -> Result<Manifest> {
    if manifest_path.exists() {
        return environment::load_manifest(manifest_path);
    }

    let absolute = if manifest_path.is_absolute() {
        manifest_path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to read the current directory")?
            .join(manifest_path)
    };
    let root_dir = absolute
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    Ok(Manifest::parse("", &absolute, root_dir)?)
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
        // Provisioning is Cloudflare-only because wrangler is the only one of the three CLIs that
        // creates a resource *and* hands back an id the manifest has to record. `cargo lambda
        // deploy` creates the function itself, and a Function App is created by `az functionapp
        // create` before anything is published to it.
        Provider::Aws => anyhow::bail!(
            "`skyzen provision` has no AWS implementation: `skyzen deploy --provider aws` creates \
             the function, and its queues and buckets are created by Terraform, CloudFormation or \
             the AWS console"
        ),
        Provider::Azure => anyhow::bail!(
            "`skyzen provision` has no Azure implementation: create the Function App with `az \
             functionapp create` (stack: .NET / Custom Handler), then run `skyzen deploy \
             --provider azure`"
        ),
        Provider::Cloudflare => {
            let manifest = environment::load_manifest(manifest_path)?;
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
        Action::Dev { .. } => "dev",
        Action::Deploy => "deploy",
        Action::Logs { .. } => "logs",
        Action::Secret(_) => "secret",
        Action::Migrate { status: true, .. } => "migrate status",
        Action::Migrate { status: false, .. } => "migrate",
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
    // Checking every provider by default would fail a Cloudflare project for not having the Azure
    // tools installed, so an unqualified `skyzen doctor` checks the two that need no account.
    let providers =
        provider.map_or_else(|| vec![Provider::Native, Provider::Cloudflare], |p| vec![p]);
    let mut failures = 0_usize;

    for provider in &providers {
        for tool in required_tools(*provider) {
            if tool.is_present() {
                output::ok(format!("{}: found `{}`", provider.as_str(), tool.label()));
            } else {
                output::failed(format!(
                    "{}: `{}` not found; install it with {}",
                    provider.as_str(),
                    tool.label(),
                    tool.remedy
                ));
                failures += 1;
            }
        }
    }

    if providers.contains(&Provider::Cloudflare) {
        failures += check_wasm_target();
        failures += check_wrangler_auth();
    }

    if providers.contains(&Provider::Azure) {
        failures += azure::check_linux_target(manifest_path);
    }

    failures += check_manifest(manifest_path, &providers, environment);

    if failures == 0 {
        Ok(())
    } else {
        anyhow::bail!("{failures} check(s) failed; see the report above")
    }
}

/// An external tool a provider's pipeline shells out to.
///
/// A cargo subcommand cannot be probed by running its own binary — `cargo lambda` is `cargo` with
/// an argument — so a check is a program *and* the arguments that make it answer.
#[derive(Debug, Clone, Copy)]
pub struct Tool {
    /// The program to run.
    program: &'static str,
    /// The arguments that make it print its version and exit zero.
    args: &'static [&'static str],
    /// How to install it, named in the failure so a first run is not a search.
    remedy: &'static str,
}

impl Tool {
    /// How the tool is spelled in a report.
    fn label(self) -> String {
        if self.args.len() > 1 {
            format!("{} {}", self.program, self.args[0])
        } else {
            self.program.to_owned()
        }
    }

    /// Whether the tool is installed and runnable.
    fn is_present(self) -> bool {
        Command::new(self.program)
            .args(self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// The tool `cargo` itself is, which every provider needs.
const CARGO: Tool = Tool {
    program: "cargo",
    args: &["--version"],
    remedy: "https://rustup.rs",
};

const fn required_tools(provider: Provider) -> &'static [Tool] {
    match provider {
        Provider::Native => &[CARGO],
        Provider::Cloudflare => &[
            CARGO,
            Tool {
                program: "wrangler",
                args: &["--version"],
                remedy: "`npm install -g wrangler`",
            },
        ],
        Provider::Aws => &[
            CARGO,
            Tool {
                program: "cargo",
                args: &["lambda", "--version"],
                remedy: "`cargo install cargo-lambda` (or `brew install cargo-lambda`)",
            },
        ],
        Provider::Azure => &[
            CARGO,
            Tool {
                program: "func",
                args: &["--version"],
                remedy: "`npm install -g azure-functions-core-tools@4 --unsafe-perm true`",
            },
        ],
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
        // Every other provider can deploy from defaults; Azure cannot, because nothing can infer
        // which Function App to publish to. Reported here so a missing file and a missing section
        // fail the same way rather than one of them passing.
        if providers.contains(&Provider::Azure) {
            output::failed(
                "azure: there is no Skyzen.toml, so nothing names the Function App to publish to",
            );
            return 1;
        }
        return 0;
    }

    let manifest = match environment::load_manifest(manifest_path) {
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
    if providers.contains(&Provider::Native) {
        report_native_wiring(&manifest);
    }
    if providers.contains(&Provider::Cloudflare) {
        failures += check_cloudflare_manifest(&manifest, environment);
    }
    if providers.contains(&Provider::Aws) {
        failures += aws::check_manifest(&manifest);
    }
    if providers.contains(&Provider::Azure) {
        failures += azure::check_manifest(&manifest);
    }
    failures
}

/// Report the native wiring: which backend each portable capability resolves to, and which of the
/// variables it reads are actually available.
///
/// A warning rather than a failure, and it counts none: `doctor` is not a run, and the machine a
/// project is diagnosed on is routinely not the one holding the production connection strings.
/// `skyzen dev` is where an unset variable is an error, because that is where it would panic.
fn report_native_wiring(manifest: &Manifest) {
    if let Some(native) = manifest.data().native.as_ref() {
        for (name, service) in &native.service {
            output::ok(format!(
                "native: service `{name}` is backed by `{}`",
                service.backend().as_str()
            ));
        }
        for (name, database) in &native.database {
            output::ok(format!(
                "native: database `{name}` is backed by `{}`",
                database.backend().as_str()
            ));
        }
    }

    report_runtime_variables(manifest, VariableKind::ALL, "native", "skyzen dev");
}

/// Report the runtime variables one provider is responsible for, and which of them have a value
/// here.
///
/// A warning rather than a failure, and it counts none: `doctor` is not a run, and the machine a
/// project is diagnosed on is routinely not the one holding the production connection strings. The
/// command named in `refused_by` is where an unset variable *is* an error, because that is where
/// it would either panic or ship a half-configured deployment.
pub fn report_runtime_variables(
    manifest: &Manifest,
    kinds: &[VariableKind],
    label: &str,
    refused_by: &str,
) {
    let variables = environment::runtime_variables_of(manifest.data(), kinds);
    if variables.is_empty() {
        return;
    }
    let loaded = environment::Environment::load(manifest.root_dir()).unwrap_or_else(|error| {
        output::warn(format!("{label}: {error:#}"));
        environment::Environment::default()
    });
    for variable in variables {
        match loaded.get(variable.name.as_str()) {
            Ok(Some(_)) => output::ok(format!(
                "{label}: {} {} is set (declared by {})",
                variable.kind.label(),
                variable.name,
                variable.declared_by
            )),
            Ok(None) => output::warn(format!(
                "{label}: {} {} is set nowhere (declared by {}); `{refused_by}` will refuse to run",
                variable.kind.label(),
                variable.name,
                variable.declared_by
            )),
            Err(error) => output::warn(format!("{label}: {} {error}", variable.name)),
        }
    }
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

/// The runtime variables one provider needs, resolved, plus the environment its child gets.
#[derive(Debug, Default)]
pub struct ChildEnvironment {
    /// Every variable of the requested kinds, with the value found for it.
    ///
    /// A provider that delivers variables to a deployed function reads these; one that only runs
    /// the application locally needs nothing beyond the resolution having succeeded. The
    /// Cloudflare sinks resolve through [`resolve_variables`] instead, because what they deliver
    /// is not what their child process runs with.
    pub resolved: ResolvedVariables,
    /// What to add to the child process's inherited environment.
    pub child_env: Vec<(String, String)>,
}

/// Resolve the runtime variables of the given kinds, and say what a child process should be
/// started with.
///
/// The child gets only the dotenv entries the CLI's own environment does not already hold, so a
/// one-off `CACHE_URL=... skyzen dev` beats `.env` rather than the other way round.
///
/// # Errors
///
/// Fails when a dotenv file cannot be read, or when a declared variable is set nowhere.
pub fn prepare_child_environment(
    manifest: &Manifest,
    kinds: &[VariableKind],
) -> Result<ChildEnvironment> {
    let loaded = environment::Environment::load(manifest.root_dir())
        .context("failed to load the project's .env files")?;
    let resolved = loaded.resolve(&environment::runtime_variables_of(manifest.data(), kinds))?;
    Ok(ChildEnvironment {
        resolved,
        child_env: loaded.child_overrides(),
    })
}

/// Resolve the runtime variables of the given kinds that `wanted` accepts.
///
/// The filter is what lets a provider leave out a variable it must not demand: a Cloudflare
/// deployment binds a Secrets Store secret rather than uploading it, so requiring the value
/// locally would refuse a deployment whose configuration is complete.
///
/// # Errors
///
/// Fails when a dotenv file cannot be read, or when a wanted variable is set nowhere.
pub fn resolve_variables(
    manifest: &Manifest,
    kinds: &[VariableKind],
    wanted: impl Fn(&RuntimeVariable) -> bool,
) -> Result<ResolvedVariables> {
    let loaded = environment::Environment::load(manifest.root_dir())
        .context("failed to load the project's .env files")?;
    let mut variables = environment::runtime_variables_of(manifest.data(), kinds);
    variables.retain(wanted);
    loaded.resolve(&variables)
}

/// Refuse a `secret set` for a name the manifest does not declare.
///
/// A name is what the application reads through its generated `Secret` extractor, so a name
/// nothing declares would upload a value no code can reach — and the typo would only show up as a
/// missing secret at cold start.
fn ensure_declared_secret(manifest: &SkyzenManifest, name: &VarName) -> Result<()> {
    if manifest.secret.iter().any(|entry| entry.name == *name) {
        return Ok(());
    }

    let declared = manifest
        .secret
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    if declared.is_empty() {
        anyhow::bail!(
            "`{name}` is not a declared secret: Skyzen.toml has no [[secret]] entries. Add one \
             (`[[secret]]` with `name = \"{name}\"`) so the application can read it."
        );
    }
    anyhow::bail!(
        "`{name}` is not a declared secret; Skyzen.toml declares: {}",
        declared.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::{Action, CommandPlan, CommandStdin};
    use crate::cli::Provider;
    use skyzen_manifest::VarName;
    use std::path::PathBuf;

    #[test]
    fn dev_defaults_to_native_and_deployment_defaults_to_the_cloud() {
        assert_eq!(
            Action::Dev {
                runner_args: Vec::new()
            }
            .default_provider(),
            Provider::Native
        );
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
        assert!(Action::Migrate {
            local: false,
            status: false
        }
        .requires_cloud());
        assert!(!Action::Dev {
            runner_args: Vec::new()
        }
        .requires_cloud());
        assert!(!Action::Build { release: false }.requires_cloud());
    }

    #[test]
    fn a_project_with_no_manifest_can_still_be_run_natively() {
        // `Skyzen.toml` is optional — services can be wired by hand in Rust, and the macros treat
        // a missing file as "nothing declared". Requiring one here would have broken every
        // project that never had one.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir_all(dir.path().join("src")).expect("create src");
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").expect("write main.rs");

        let missing = dir.path().join("Skyzen.toml");
        assert!(!missing.exists());

        let plan = super::prepare(
            &Action::Dev {
                runner_args: Vec::new(),
            },
            &missing,
            Some(Provider::Native),
            None,
            false,
        )
        .expect("native dev needs no manifest");
        assert!(plan.steps[0].describe().contains("cargo run"));

        // The Cloudflare path still fails, but with the message that says what to add.
        let error = super::prepare(
            &Action::Deploy,
            &missing,
            Some(Provider::Cloudflare),
            None,
            false,
        )
        .expect_err("a Worker needs a [cloudflare] section");
        assert!(error.to_string().contains("[cloudflare]"), "{error}");
    }

    #[test]
    fn a_command_renders_with_its_working_directory() {
        let plan = CommandPlan {
            program: "wrangler".to_owned(),
            args: vec!["dev".to_owned()],
            cwd: Some(PathBuf::from("/tmp/app")),
            stdin: CommandStdin::Inherit,
        };
        assert_eq!(plan.display(), "(cd /tmp/app && wrangler dev)");
    }

    #[test]
    fn what_a_command_carries_on_standard_input_is_not_part_of_its_rendering() {
        // The whole reason values travel on standard input: `--dry-run` and every progress line
        // print a command in full, and neither may print a secret.
        let plan = CommandPlan {
            program: "wrangler".to_owned(),
            args: vec!["secret".to_owned(), "bulk".to_owned()],
            cwd: None,
            stdin: CommandStdin::Inherit,
        }
        .with_stdin(secrecy::SecretString::from(
            r#"{"STRIPE_KEY":"sk_live_123"}"#,
        ));

        assert_eq!(plan.display(), "wrangler secret bulk");
        assert!(!plan.display().contains("sk_live_123"));
        assert!(!super::Step::Command(plan)
            .describe()
            .contains("sk_live_123"));
    }

    #[test]
    fn a_secret_no_entry_declares_is_refused_with_the_names_that_are() {
        let manifest = skyzen_manifest::Manifest::parse(
            "[[secret]]\nname = \"STRIPE_KEY\"\n\n[[secret]]\nname = \"JWT_SIGNING_KEY\"\n",
            "Skyzen.toml",
            "/tmp/app",
        )
        .expect("valid manifest");
        let name = |text: &str| VarName::try_from(text.to_owned()).expect("a name");

        super::ensure_declared_secret(manifest.data(), &name("STRIPE_KEY")).expect("declared");

        let error = super::ensure_declared_secret(manifest.data(), &name("STRIPE_KEYY"))
            .expect_err("a typo must not upload a value no code can read");
        let rendered = error.to_string();
        assert!(rendered.contains("STRIPE_KEY"), "{rendered}");
        assert!(rendered.contains("JWT_SIGNING_KEY"), "{rendered}");
    }

    #[test]
    fn a_project_declaring_no_secrets_says_how_to_declare_one() {
        let manifest =
            skyzen_manifest::Manifest::parse("", "Skyzen.toml", "/tmp/app").expect("empty");
        let error = super::ensure_declared_secret(
            manifest.data(),
            &VarName::try_from("STRIPE_KEY".to_owned()).expect("a name"),
        )
        .expect_err("nothing is declared");
        assert!(error.to_string().contains("[[secret]]"), "{error}");
    }
}
