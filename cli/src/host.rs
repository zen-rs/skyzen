//! The machine-side half of the native stack's acceleration: the kernel and NIC facilities a
//! serving host offers the runtime, which `skyzen doctor` reports and `skyzen deploy --provider
//! native` configures.
//!
//! Every facility is Linux's. [`linux::Host`] is the model of them — what this machine supports and
//! has enabled, read from netlink, sysfs and procfs — and what the deployment compares
//! `[native.linux]` against, so the report and the configuration cannot disagree.

#[cfg(target_os = "linux")]
pub mod linux;

use crate::providers::Step;
use anyhow::Result;
use skyzen_manifest::NativeLinuxSection;

/// Report the acceleration facilities of the machine `skyzen doctor` runs on.
///
/// No facility's state fails the doctor: the machine a project is diagnosed on is routinely not
/// the one it serves from, and a facility being off is a fact about the host, not an error in the
/// project. Returns the failed checks: one when the runtime the netlink queries run on cannot be
/// started, none otherwise.
#[cfg(target_os = "linux")]
pub fn report() -> usize {
    match crate::runtime::block_on(linux::Host::probe()) {
        Ok(host) => {
            host.report();
            0
        }
        Err(error) => {
            crate::output::failed(format!("native: {error:#}"));
            1
        }
    }
}

/// Report that the acceleration facilities are Linux's; nothing fails.
#[cfg(not(target_os = "linux"))]
pub fn report() -> usize {
    crate::output::off(format!(
        "native: kernel TLS, NIC TLS offload, header/data split, busy polling, connection \
         steering and zero-copy sends are Linux-only; this machine runs {}",
        std::env::consts::OS
    ));
    0
}

/// The steps that bring this host to `section`: one per setting it does not have yet, then the
/// check that every setting took.
///
/// # Errors
///
/// Fails when the section asks for a facility the host lacks or does not report, and when a
/// setting has to change, `dry_run` is off and the CLI is not root.
#[cfg(target_os = "linux")]
pub fn apply_steps(section: &NativeLinuxSection, dry_run: bool) -> Result<Vec<Step>> {
    let steps = crate::runtime::block_on(linux::apply::steps(section))??;
    if !dry_run && !steps.is_empty() {
        ensure_root("applying [native.linux]")?;
    }
    Ok(steps)
}

/// `[native.linux]` is Linux's; anywhere else it is refused.
///
/// # Errors
///
/// Always.
#[cfg(not(target_os = "linux"))]
pub fn apply_steps(_section: &NativeLinuxSection, _dry_run: bool) -> Result<Vec<Step>> {
    anyhow::bail!(
        "[native.linux] configures the Linux host the CLI runs on; this machine runs {}",
        std::env::consts::OS
    )
}

/// Refuse `what` unless the CLI runs as root.
///
/// # Errors
///
/// The effective user is not root.
#[cfg(unix)]
pub fn ensure_root(what: &str) -> Result<()> {
    if nix::unistd::geteuid().is_root() {
        Ok(())
    } else {
        anyhow::bail!(
            "{what} needs root; run it with sudo, or pass --dry-run to see what it would do"
        )
    }
}

/// Refuse `what`: only a Unix host has a root.
///
/// # Errors
///
/// Always.
#[cfg(not(unix))]
pub fn ensure_root(what: &str) -> Result<()> {
    anyhow::bail!("{what} needs a Linux host")
}
