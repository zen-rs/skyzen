//! Skyzen unified CLI for local emulation and deployment.

mod args;
mod deps;
mod dev;
mod manifest;
mod providers;
mod scaffold;

use anyhow::{Context, Result};
use args::{Action, CliOptions};
use providers::{prepare, PreparedRun, RunMode};
use std::{
    fs,
    process::{Command, Stdio},
};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let options = CliOptions::parse(std::env::args())?;
    if options.action == Action::New {
        return scaffold::create_project(&options);
    }
    let prepared = prepare(&options)?;
    execute(&prepared, options.dry_run)
}

fn execute(prepared: &PreparedRun, dry_run: bool) -> Result<()> {
    for file in &prepared.generated_files {
        if dry_run {
            println!("[dry-run] write {}", file.path.display());
            println!("{}", file.contents);
            continue;
        }
        if let Some(parent) = file.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&file.path, &file.contents)
            .with_context(|| format!("failed to write {}", file.path.display()))?;
        println!("[skyzen] wrote {}", file.path.display());
    }

    for command in &prepared.commands {
        let display = command.display();
        if dry_run {
            println!("[dry-run] {display}");
            continue;
        }

        println!("[skyzen] {display}");
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
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
            anyhow::bail!(
                "command failed with status {}: {}",
                status,
                command.display()
            );
        }
    }

    if prepared.commands.is_empty() {
        match prepared.action {
            Action::Doctor => {}
            Action::New => unreachable!("new is handled before prepare"),
            Action::Dev | Action::Deploy => {
                anyhow::bail!("no command prepared");
            }
        }
    }

    if prepared.run_mode == RunMode::Watch {
        let Some(command) = prepared.commands.first() else {
            anyhow::bail!("watch mode requires at least one command");
        };
        return dev::run_native_watch(command, dry_run);
    }

    Ok(())
}
