//! Skyzen unified CLI for local emulation and deployment.

mod capabilities;
mod cli;
mod dev;
mod environment;
mod migrate;
mod output;
mod project;
mod providers;
mod scaffold;
mod secret_files;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use cli::{Cli, Command, SecretCommand};
use providers::{Action, FileContents, ProviderPlan, RunMode, SecretAction};
use secrecy::{zeroize::Zeroize as _, ExposeSecret as _};
use std::{
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Command::Completions { shell } => {
            clap_complete::generate(
                *shell,
                &mut Cli::command(),
                "skyzen",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Command::New {
            path,
            template,
            force,
            overwrite,
        } => scaffold::create_project(&scaffold::ScaffoldRequest {
            path,
            template: *template,
            existing: match (*force, *overwrite) {
                (_, true) => scaffold::ExistingFiles::Replace,
                (true, false) => scaffold::ExistingFiles::Keep,
                (false, false) => scaffold::ExistingFiles::Refuse,
            },
            dry_run: cli.dry_run,
            install_dependencies: true,
        }),
        Command::Add {
            capabilities: names,
            list,
        } => capabilities::add(&project_root(&cli.manifest)?, names, *list || cli.dry_run),
        Command::Provision => {
            providers::provision(&cli.manifest, cli.provider, cli.env.as_deref(), cli.dry_run)
        }
        Command::Doctor => providers::doctor(&cli.manifest, cli.provider, cli.env.as_deref()),
        // `migrate` is the one action with two genuinely different implementations rather than two
        // renderings of one plan: the Cloudflare path shells out to wrangler and is planned like
        // everything else, while the native path opens a connection and applies the files itself.
        // `migrate::dispatch` picks between them, and refuses the combinations that would answer a
        // different question than the one typed.
        Command::Migrate { command, local } => match migrate::dispatch(cli.provider, *local)? {
            migrate::Dispatch::Native => migrate::run(&cli.manifest, *command, cli.dry_run),
            migrate::Dispatch::Provider => run_plan(
                &cli,
                &Action::Migrate {
                    local: *local,
                    status: command.is_some(),
                },
            ),
        },
        command => run_plan(&cli, &action_for(command)?),
    }
}

/// Build the provider's plan for `action` and execute it.
fn run_plan(cli: &Cli, action: &Action) -> Result<()> {
    let plan = providers::prepare(
        action,
        &cli.manifest,
        cli.provider,
        cli.env.as_deref(),
        cli.dry_run,
    )?;
    execute(&plan, cli.dry_run)
}

/// The directory `cargo add` should run in.
fn project_root(manifest_path: &Path) -> Result<PathBuf> {
    let absolute = if manifest_path.is_absolute() {
        manifest_path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to read the current directory")?
            .join(manifest_path)
    };
    Ok(absolute
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf))
}

fn action_for(command: &Command) -> Result<Action> {
    Ok(match command {
        Command::Build { release } => Action::Build { release: *release },
        Command::Dev { runner_args } => Action::Dev {
            runner_args: runner_args.clone(),
        },
        Command::Deploy => Action::Deploy,
        Command::Logs { wrangler_args } => Action::Logs {
            wrangler_args: wrangler_args.clone(),
        },
        Command::Secret { command } => Action::Secret(match command {
            // Read here rather than in a provider: every provider then delivers the same value the
            // same way, and none of them has to know how a terminal prompts for one.
            SecretCommand::Set { name } => SecretAction::Set {
                name: name.clone(),
                value: read_secret_value(name)?,
            },
            SecretCommand::Push => SecretAction::Push,
            SecretCommand::List => SecretAction::List,
        }),
        // `Migrate` belongs here too: `run` picks between the native runner and the provider plan
        // itself, and builds the `Action` on the branch that needs one.
        Command::New { .. }
        | Command::Add { .. }
        | Command::Provision
        | Command::Doctor
        | Command::Migrate { .. }
        | Command::Completions { .. } => {
            unreachable!("handled before dispatch")
        }
    })
}

/// Read one secret's value from the CLI's own standard input.
///
/// All of it, with the trailing newline a `printf`, an `echo` or a here-string leaves behind
/// removed — the same trimming wrangler does — because a value with an accidental newline is
/// rejected by whatever it is eventually sent to, hours later.
fn read_secret_value(name: &skyzen_manifest::VarName) -> Result<secrecy::SecretString> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .with_context(|| format!("failed to read the value for `{name}` from standard input"))?;
    let value = secrecy::SecretString::from(buffer.trim_end_matches(['\n', '\r']).to_owned());
    buffer.zeroize();

    if value.expose_secret().is_empty() {
        anyhow::bail!(
            "no value for `{name}` on standard input; pipe one in, as in `printf %s \"$VALUE\" | \
             skyzen secret set {name}`"
        );
    }
    Ok(value)
}

fn execute(plan: &ProviderPlan, dry_run: bool) -> Result<()> {
    // `deploy --dry-run` is the exception: it maps onto `wrangler deploy --dry-run`, which
    // validates the real bundle, so the plan runs for real and only the upload is skipped.
    let simulate = dry_run && !plan.execute_despite_dry_run;

    for file in &plan.generated_files {
        if simulate {
            println!("{}", dry_run_report(file));
            continue;
        }
        write_generated_file(file)?;
        output::step(format!("wrote {}", file.path.display()));
    }

    if let Some(build) = &plan.build {
        if simulate {
            output::dry_run(build.describe());
        } else {
            output::step(build.describe());
            build.run()?;
        }
    }

    if plan.run_mode == RunMode::Once {
        return providers::run_steps(&plan.steps, simulate, &plan.child_env);
    }

    // Everything a supervised process needs done first — seeding a local store, say — is a step
    // ahead of it, and runs before it starts.
    let (preamble, command) = plan.supervised()?;
    providers::run_steps(preamble, simulate, &plan.child_env)?;
    if simulate {
        output::dry_run(format!("supervise {}", command.display()));
        return Ok(());
    }
    let watch_root = plan
        .watch_root
        .as_deref()
        .context("a supervised run needs a directory to watch")?;
    dev::supervise(&dev::Supervision {
        command,
        build: plan.build.as_deref(),
        mode: plan.run_mode,
        child_env: &plan.child_env,
        watch_root,
    })
}

/// What `--dry-run` says about one generated file.
///
/// A pure function so the promise it makes — that a secret's value never reaches stdout — is
/// something a test can hold it to, rather than something a reader has to trust the printing code
/// about.
fn dry_run_report(file: &providers::GeneratedFile) -> String {
    match &file.contents {
        FileContents::Public(contents) => {
            format!("[dry-run] write {}\n{contents}", file.path.display())
        }
        FileContents::Secret(_) => format!(
            "[dry-run] write {} (secret values, not shown)",
            file.path.display()
        ),
    }
}

/// Write one generated file, creating its directory.
///
/// A file holding values is created readable by its owner alone. Doing it in the `OpenOptions`
/// rather than with a `set_permissions` afterwards means there is no window in which the values
/// are on disk under the default mode.
fn write_generated_file(file: &providers::GeneratedFile) -> Result<()> {
    if let Some(parent) = file.path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let (contents, owner_only) = match &file.contents {
        FileContents::Public(contents) => (contents.as_str(), false),
        FileContents::Secret(secret) => (secret.expose_secret(), true),
    };

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if owner_only {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = owner_only;

    let mut handle = options
        .open(&file.path)
        .with_context(|| format!("failed to write {}", file.path.display()))?;
    handle
        .write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {}", file.path.display()))
}

#[cfg(test)]
mod tests {
    use super::dry_run_report;
    use crate::providers::{FileContents, GeneratedFile};
    use std::path::PathBuf;

    #[test]
    fn a_dry_run_prints_configuration_verbatim() {
        let report = dry_run_report(&GeneratedFile {
            path: PathBuf::from("/tmp/app/wrangler.toml"),
            contents: FileContents::Public("name = \"demo\"".to_owned()),
        });
        assert!(report.contains("write /tmp/app/wrangler.toml"), "{report}");
        assert!(report.contains("name = \"demo\""), "{report}");
    }

    #[test]
    fn a_dry_run_never_prints_a_value() {
        let report = dry_run_report(&GeneratedFile {
            path: PathBuf::from("/tmp/app/.dev.vars"),
            contents: FileContents::Secret("STRIPE_KEY=sk_live_123".into()),
        });
        assert_eq!(
            report,
            "[dry-run] write /tmp/app/.dev.vars (secret values, not shown)"
        );
        assert!(!report.contains("sk_live_123"), "{report}");
    }
}
