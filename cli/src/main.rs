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

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use cli::{Cli, Command};
use providers::{Action, ProviderPlan, RunMode};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command as Process, Stdio},
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
        Command::Migrate { command, local } => {
            match migrate::dispatch(cli.provider, command.is_some(), *local)? {
                migrate::Dispatch::Native => migrate::run(&cli.manifest, *command, cli.dry_run),
                migrate::Dispatch::Provider => run_plan(&cli, &Action::Migrate { local: *local }),
            }
        }
        command => run_plan(&cli, &action_for(command)),
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

fn action_for(command: &Command) -> Action {
    match command {
        Command::Build { release } => Action::Build { release: *release },
        Command::Dev { runner_args } => Action::Dev {
            runner_args: runner_args.clone(),
        },
        Command::Deploy => Action::Deploy,
        Command::Logs { wrangler_args } => Action::Logs {
            wrangler_args: wrangler_args.clone(),
        },
        Command::Secret { command } => Action::Secret(command.clone()),
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
    }
}

fn execute(plan: &ProviderPlan, dry_run: bool) -> Result<()> {
    // `deploy --dry-run` is the exception: it maps onto `wrangler deploy --dry-run`, which
    // validates the real bundle, so the plan runs for real and only the upload is skipped.
    let simulate = dry_run && !plan.execute_despite_dry_run;

    for file in &plan.generated_files {
        if simulate {
            output::dry_run(format!("write {}", file.path.display()));
            println!("{}", file.contents);
            continue;
        }
        if let Some(parent) = file.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&file.path, &file.contents)
            .with_context(|| format!("failed to write {}", file.path.display()))?;
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
        return run_commands(&plan.commands, simulate, &plan.child_env);
    }

    let Some(command) = plan.commands.first() else {
        anyhow::bail!("a supervised run needs at least one command");
    };
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

fn run_commands(
    commands: &[providers::CommandPlan],
    simulate: bool,
    child_env: &[(String, String)],
) -> Result<()> {
    for command in commands {
        let display = command.display();
        if simulate {
            output::dry_run(display);
            continue;
        }

        output::step(&display);
        let mut process = Process::new(&command.program);
        process
            .args(&command.args)
            .envs(child_env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Some(cwd) = &command.cwd {
            process.current_dir(cwd);
        }

        let status = process
            .status()
            .with_context(|| format!("failed to launch {}", command.program))?;
        if !status.success() {
            anyhow::bail!("command failed with status {status}: {display}");
        }
    }

    Ok(())
}
