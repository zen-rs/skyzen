//! The machine-side half of the native stack's acceleration: the kernel and NIC facilities a
//! serving host offers the runtime, which `skyzen doctor` reports.
//!
//! Every facility is Linux's. [`linux::Host`] is the model of them — what this machine supports and
//! has enabled, read from netlink, sysfs and procfs — and what a configuring command compares the
//! manifest's settings against.

#[cfg(target_os = "linux")]
pub mod linux;

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
