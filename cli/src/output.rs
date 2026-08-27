//! The CLI's user-facing output.
//!
//! `skyzen` is a foreground tool a person watches, so its progress belongs on stdout rather than
//! in a `tracing` subscriber the user would have to enable. Routing every line through here keeps
//! the prefixes consistent and gives the dry-run paths one place to hook.

use std::fmt::Display;

/// Prefix on every line the CLI writes, so its output is distinguishable from the output of the
/// tools it drives (cargo, wrangler, the application itself).
const PREFIX: &str = "[skyzen]";

/// Report an action the CLI is about to take.
pub fn step(message: impl Display) {
    println!("{PREFIX} {message}");
}

/// Report an action the CLI would take, but is not taking because of `--dry-run`.
pub fn dry_run(message: impl Display) {
    println!("[dry-run] {message}");
}

/// Report a check that passed.
pub fn ok(message: impl Display) {
    println!("[ok] {message}");
}

/// Report a check that failed. The caller is expected to fail the command afterwards.
pub fn failed(message: impl Display) {
    println!("[fail] {message}");
}

/// Report something the user should know but that is not a failure.
pub fn warn(message: impl Display) {
    eprintln!("{PREFIX} warning: {message}");
}
