//! The `skyzen dev` supervisor.
//!
//! One loop serves both providers. Native dev restarts the child on every debounced change;
//! Cloudflare dev leaves `wrangler dev` running and re-runs the wasm build instead, because
//! wrangler reloads the regenerated bundle on its own and restarting it would drop local state
//! and rebind the port.

use crate::{
    environment, output,
    providers::{cloudflare::build, cloudflare::build::BuildPlan, CommandPlan, RunMode},
};
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode, DebounceEventResult};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    time::Duration,
};

/// How long to coalesce filesystem events before acting on them.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// How often the loop wakes to notice the child exited on its own.
const POLL: Duration = Duration::from_millis(250);

/// How long a child gets to exit after `SIGINT` before it is killed.
const GRACE: Duration = Duration::from_secs(2);

enum Signal {
    FileEvents(DebounceEventResult),
    Shutdown,
}

/// Everything the supervisor needs.
#[derive(Debug)]
pub struct Supervision<'a> {
    /// The process to run.
    pub command: &'a CommandPlan,
    /// The build to re-run under [`RunMode::Rebuild`].
    pub build: Option<&'a BuildPlan>,
    /// Whether a change restarts the child or re-runs the build.
    pub mode: RunMode,
    /// Environment for the child, from the project's `.env` files.
    pub child_env: &'a [(String, String)],
    /// The directory to watch.
    pub watch_root: &'a Path,
}

/// A supervised child that is reclaimed on every exit path.
///
/// The loop used to terminate the child only on the shutdown branch, so a watcher error or a
/// disconnected channel returned with the server still holding its listening socket — and the next
/// `skyzen dev` failed to bind. A `Drop` guard covers `?`, `break` and panics alike.
struct SupervisedChild {
    child: Option<Child>,
}

impl SupervisedChild {
    fn spawn(command: &CommandPlan, child_env: &[(String, String)]) -> Result<Self> {
        output::step(command.display());
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .envs(child_env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Some(cwd) = &command.cwd {
            process.current_dir(cwd);
        }
        let child = process
            .spawn()
            .with_context(|| format!("failed to launch {}", command.program))?;
        Ok(Self { child: Some(child) })
    }

    /// Whether the child has exited on its own, reporting its status once.
    fn report_if_exited(&mut self) -> Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        if let Some(status) = child.try_wait()? {
            output::step(format!(
                "process exited with status {status}, waiting for changes"
            ));
            self.child = None;
        }
        Ok(())
    }

    fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        terminate(&mut child);
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Run the supervision loop.
///
/// # Errors
///
/// Fails when the watcher cannot be installed or the child cannot be launched. A watcher *event*
/// error is reported and the loop continues: losing one filesystem event is not a reason to tear
/// down a working dev session.
pub fn supervise(supervision: &Supervision<'_>) -> Result<()> {
    let ignore = build_ignore_matcher(supervision.watch_root)?;
    let (signal_tx, signal_rx) = mpsc::channel();
    let fs_tx = signal_tx.clone();
    ctrlc::set_handler(move || {
        let _ = signal_tx.send(Signal::Shutdown);
    })
    .context("failed to install Ctrl+C handler")?;

    let mut debouncer = new_debouncer(DEBOUNCE, None, move |result| {
        let _ = fs_tx.send(Signal::FileEvents(result));
    })
    .context("failed to initialize file watcher")?;

    for watch_path in watch_paths(supervision.watch_root) {
        let mode = if watch_path.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        debouncer
            .watch(&watch_path, mode)
            .with_context(|| format!("failed to watch {}", watch_path.display()))?;
    }

    let mut child = SupervisedChild::spawn(supervision.command, supervision.child_env)?;
    loop {
        match signal_rx.recv_timeout(POLL) {
            Ok(Signal::FileEvents(result)) => {
                if !has_relevant_change(result, &ignore) {
                    continue;
                }
                on_change(supervision, &mut child)?;
            }
            Err(RecvTimeoutError::Timeout) => child.report_if_exited()?,
            // Ctrl+C, or the watcher thread going away. Either way the guard reclaims the child.
            Ok(Signal::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

fn on_change(supervision: &Supervision<'_>, child: &mut SupervisedChild) -> Result<()> {
    match supervision.mode {
        RunMode::Restart | RunMode::Once => {
            output::step("change detected, restarting");
            child.terminate();
            *child = SupervisedChild::spawn(supervision.command, supervision.child_env)?;
        }
        RunMode::Rebuild => {
            let Some(plan) = supervision.build else {
                output::warn("change detected, but this run has no build step to repeat");
                return Ok(());
            };
            output::step("change detected, rebuilding worker artifacts");
            // A rebuild failure is a compile error the user is about to fix, not a reason to stop
            // supervising: wrangler keeps serving the previous bundle until the next success.
            if let Err(error) = build::run(plan) {
                output::warn(format!("rebuild failed: {error:#}"));
            }
        }
    }
    Ok(())
}

fn terminate(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }

    #[cfg(unix)]
    {
        use nix::{
            sys::signal::{kill, Signal},
            unistd::Pid,
        };
        use std::{thread, time::Instant};

        if let Ok(raw) = i32::try_from(child.id()) {
            let _ = kill(Pid::from_raw(raw), Signal::SIGINT);
            let deadline = Instant::now() + GRACE;
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

fn watch_paths(root: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![
        root.join("src"),
        root.join("examples"),
        root.join("tests"),
        root.join("Skyzen.toml"),
        root.join("Cargo.toml"),
    ];
    // An edited `.env` changes what the child sees, so it belongs in the watch set too.
    candidates.extend(environment::dotenv_paths(root));

    candidates.retain(|path| path.exists());
    candidates.sort();
    candidates.dedup();
    candidates
}

fn build_ignore_matcher(root: &Path) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    let gitignore_path = root.join(".gitignore");
    if gitignore_path.exists() {
        builder
            .add(&gitignore_path)
            .with_context(|| format!("failed to load {}", gitignore_path.display()))?;
    }

    for pattern in ["target", ".git", ".skyzen", "node_modules", "dist"] {
        builder.add_line(None, pattern)?;
    }
    builder.build().context("failed to build ignore matcher")
}

/// Whether a batch of filesystem events contains anything worth acting on.
///
/// A watcher error is reported and treated as "nothing changed" rather than propagated: the
/// previous code returned from the supervisor here, abandoning the running child.
fn has_relevant_change(result: DebounceEventResult, ignore: &Gitignore) -> bool {
    let events = match result {
        Ok(events) => events,
        Err(errors) => {
            for error in errors {
                output::warn(format!("file watch error: {error}"));
            }
            return false;
        }
    };

    events.iter().flat_map(|event| &event.paths).any(|path| {
        !ignore
            .matched_path_or_any_parents(path, path.is_dir())
            .is_ignore()
    })
}

#[cfg(test)]
mod tests {
    use super::{build_ignore_matcher, watch_paths};

    #[test]
    fn generated_and_vendored_directories_are_ignored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ignore = build_ignore_matcher(dir.path()).expect("ignore matcher");
        for ignored in [
            "target/foo.rs",
            ".skyzen/gen/wrangler.toml",
            "dist/worker.js",
        ] {
            assert!(
                ignore
                    .matched_path_or_any_parents(dir.path().join(ignored), false)
                    .is_ignore(),
                "{ignored} should be ignored"
            );
        }
        assert!(!ignore
            .matched_path_or_any_parents(dir.path().join("src/main.rs"), false)
            .is_ignore());
    }

    #[test]
    fn the_watch_set_covers_sources_the_manifest_and_the_env_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).expect("create src");
        std::fs::write(root.join("Cargo.toml"), "").expect("write Cargo.toml");
        std::fs::write(root.join("Skyzen.toml"), "").expect("write Skyzen.toml");
        std::fs::write(root.join(".env"), "").expect("write .env");

        let watched = watch_paths(root);
        assert!(watched.contains(&root.join("src")));
        assert!(watched.contains(&root.join("Cargo.toml")));
        assert!(watched.contains(&root.join("Skyzen.toml")));
        assert!(watched.contains(&root.join(".env")));
        // Nothing that does not exist is registered, or the watcher fails to install.
        assert!(!watched.contains(&root.join("tests")));
    }
}
