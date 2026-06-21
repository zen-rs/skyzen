use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    time::Duration,
};

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode, DebounceEventResult};

use crate::providers::CommandPlan;

enum DevSignal {
    FileEvents(DebounceEventResult),
    Shutdown,
}

pub fn run_native_watch(command: &CommandPlan, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("[dry-run] native watch {}", command.display());
        return Ok(());
    }

    let cwd = command.cwd.clone().unwrap_or(std::env::current_dir()?);
    let ignore = build_ignore_matcher(&cwd)?;
    let (signal_tx, signal_rx) = mpsc::channel();
    let fs_tx = signal_tx.clone();
    ctrlc::set_handler(move || {
        let _ = signal_tx.send(DevSignal::Shutdown);
    })
    .context("failed to install Ctrl+C handler")?;

    let mut debouncer = new_debouncer(Duration::from_millis(300), None, move |result| {
        let _ = fs_tx.send(DevSignal::FileEvents(result));
    })
    .context("failed to initialize file watcher")?;

    for watch_path in watch_paths(&cwd) {
        let mode = if watch_path.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        debouncer
            .watch(&watch_path, mode)
            .with_context(|| format!("failed to watch {}", watch_path.display()))?;
    }

    let mut child = spawn_command(command)?;
    loop {
        match signal_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(DevSignal::Shutdown) => {
                terminate_child(&mut child)?;
                break;
            }
            Ok(DevSignal::FileEvents(result)) => {
                let changed = relevant_paths(result, &ignore)?;
                if changed.is_empty() {
                    continue;
                }

                println!("[skyzen] change detected, restarting");
                terminate_child(&mut child)?;
                child = spawn_command(command)?;
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait()? {
                    println!("[skyzen] process exited with status {status}, waiting for changes");
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

fn spawn_command(command: &CommandPlan) -> Result<Child> {
    println!("[skyzen] {}", command.display());
    let mut child = Command::new(&command.program);
    child
        .args(&command.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(cwd) = &command.cwd {
        child.current_dir(cwd);
    }
    child
        .spawn()
        .with_context(|| format!("failed to launch {}", command.program))
}

fn terminate_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        use nix::{
            sys::signal::{kill, Signal},
            unistd::Pid,
        };
        use std::{thread, time::Instant};

        let pid = Pid::from_raw(
            i32::try_from(child.id()).expect("child process id exceeded i32 range on unix"),
        );
        let _ = kill(pid, Signal::SIGINT);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn watch_paths(root: &Path) -> Vec<PathBuf> {
    let candidates = [
        root.join("src"),
        root.join("examples"),
        root.join("tests"),
        root.join("Skyzen.toml"),
        root.join("Cargo.toml"),
    ];

    candidates
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

fn build_ignore_matcher(root: &Path) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    let gitignore_path = root.join(".gitignore");
    if gitignore_path.exists() {
        builder
            .add(gitignore_path)
            .with_context(|| format!("failed to load {}", root.join(".gitignore").display()))?;
    }

    builder.add_line(None, "target")?;
    builder.add_line(None, ".git")?;
    builder.add_line(None, ".skyzen")?;
    builder.add_line(None, "node_modules")?;
    builder.build().context("failed to build ignore matcher")
}

fn relevant_paths(result: DebounceEventResult, ignore: &Gitignore) -> Result<Vec<PathBuf>> {
    let events = result.map_err(|errors| {
        anyhow::anyhow!(
            "watch error(s): {}",
            errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let mut changed = Vec::new();
    for event in events {
        for path in &event.paths {
            let is_dir = path.is_dir();
            let matched = ignore.matched_path_or_any_parents(path, is_dir);
            if matched.is_ignore() {
                continue;
            }
            changed.push(path.clone());
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_target_directory() {
        let root = std::env::temp_dir();
        let ignore = build_ignore_matcher(&root).expect("ignore matcher");
        let matched = ignore.matched_path_or_any_parents(root.join("target/foo.rs"), false);
        assert!(matched.is_ignore());
    }

    #[test]
    fn watch_paths_include_existing_project_files() {
        let root = std::env::temp_dir().join("skyzen-dev-watch-paths");
        std::fs::create_dir_all(root.join("src")).expect("create src");
        std::fs::write(root.join("Cargo.toml"), "").expect("write Cargo.toml");

        let watched = watch_paths(&root);
        assert!(watched.contains(&root.join("src")));
        assert!(watched.contains(&root.join("Cargo.toml")));
    }
}
