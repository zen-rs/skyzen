//! Steering each connection to the serving thread on the CPU that received it (Linux).
//!
//! Every serving thread has a `SO_REUSEPORT` listener of its own, and by default the kernel hashes
//! a new connection to one of them, regardless of which core the NIC queue that received its
//! packets interrupts: the packets are handled on one core and the requests on another. With
//! steering on, thread `i` is pinned to the `i`-th CPU of the process's affinity mask, its
//! listener reports that CPU through `SO_INCOMING_CPU`, and one classic BPF program attached to
//! the reuseport group (`SO_ATTACH_REUSEPORT_CBPF`) picks the listener of the CPU the SYN arrived
//! on. A CPU no listener serves falls back to the kernel's hash.
//!
//! Steering pays only when every serving CPU takes a NIC receive queue's interrupt, so
//! [`Steering::Auto`] turns it on exactly then; the operator forces it either way with
//! `--steering` or `SKYZEN_STEERING`, and forcing it on where the layout cannot support it is a
//! refusal to start. Off Linux there is nothing to steer with: the setting is accepted, `auto` and
//! `off` change nothing, and `on` is a refusal.

use std::{fmt, str::FromStr};

use tracing::info;

use super::ListenAddr;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(test, target_os = "linux"))]
pub(super) use linux::pin_current_thread;
#[cfg(target_os = "linux")]
pub(super) use linux::set_incoming_cpu;
#[cfg(target_os = "linux")]
pub use skyzen_core::steering::{rx_layout, rx_layout_at, Placement, RxLayout, RxQueue};

/// Whether connections are steered to the thread on the CPU that received them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Steering {
    /// On when every serving thread's CPU takes a NIC receive queue's interrupt, off otherwise.
    #[default]
    Auto,
    /// Always; a start where the layout cannot support it fails.
    On,
    /// Never.
    Off,
}

impl FromStr for Steering {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "on" | "true" | "1" => Ok(Self::On),
            "off" | "false" | "0" => Ok(Self::Off),
            other => Err(format!(
                "`{other}` is not a steering mode; use `auto`, `on` or `off`"
            )),
        }
    }
}

/// The steering `--steering <auto|on|off>` or `SKYZEN_STEERING` asks for; the flag wins.
///
/// # Panics
///
/// A value that is not `auto`, `on` or `off` kills the process at startup, like every other
/// malformed setting.
#[must_use]
pub fn apply_steering_override(args: impl IntoIterator<Item = String>) -> Steering {
    const ENV: &str = "SKYZEN_STEERING";

    let mut args = args.into_iter().skip(1);
    let mut flag = None;
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--steering=") {
            flag = Some(value.to_owned());
        } else if arg == "--steering" {
            flag =
                Some(args.next().unwrap_or_else(|| {
                    panic!("--steering expects `auto`, `on` or `off` after it")
                }));
        }
    }
    let (value, origin) = match flag {
        Some(value) => (value, "--steering"),
        None => match std::env::var(ENV) {
            Ok(value) => (value, ENV),
            Err(std::env::VarError::NotPresent) => return Steering::Auto,
            Err(error) => panic!("{ENV} is not valid text: {error}"),
        },
    };
    let steering = value
        .parse()
        .unwrap_or_else(|error| panic!("invalid {origin}: {error}"));
    info!("Connection steering set to `{value}` by {origin}");
    steering
}

/// Why steering cannot be had.
#[derive(Debug)]
pub(super) enum SteeringError {
    /// Forced on, but the kernel facility is Linux's.
    #[cfg(not(target_os = "linux"))]
    NotLinux,
    /// Forced on, but the listener is a unix socket, which no NIC queue delivers to.
    #[cfg(target_os = "linux")]
    NotTcp,
    /// Forced on, but this platform's host owns the sockets.
    NotServer,
    /// The process's CPU affinity cannot be read, or has fewer CPUs than serving threads.
    #[cfg(target_os = "linux")]
    Affinity(std::io::Error),
    /// The NIC receive-queue layout cannot be read.
    #[cfg(target_os = "linux")]
    Layout(std::io::Error),
    /// Forced on, but these serving CPUs take no NIC receive queue's interrupt.
    #[cfg(target_os = "linux")]
    Unserved(Vec<usize>),
    /// A thread could not be pinned.
    #[cfg(target_os = "linux")]
    Pin {
        /// The CPU asked for.
        cpu: usize,
        /// What the system said.
        error: std::io::Error,
    },
}

impl fmt::Display for SteeringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const FORCED: &str = "connection steering is forced on (--steering or SKYZEN_STEERING)";
        match self {
            #[cfg(not(target_os = "linux"))]
            Self::NotLinux => write!(f, "{FORCED}, but only Linux can steer connections"),
            #[cfg(target_os = "linux")]
            Self::NotTcp => write!(f, "{FORCED}, but the listener is a unix socket"),
            Self::NotServer => write!(
                f,
                "{FORCED}, but this platform owns the listening sockets; unset it or run where \
                 it applies"
            ),
            #[cfg(target_os = "linux")]
            Self::Affinity(error) => write!(f, "cannot pin the serving threads: {error}"),
            #[cfg(target_os = "linux")]
            Self::Layout(error) => {
                write!(
                    f,
                    "{FORCED}, but the NIC queue layout cannot be read: {error}"
                )
            }
            #[cfg(target_os = "linux")]
            Self::Unserved(cpus) => write!(
                f,
                "{FORCED}, but CPUs {cpus:?} take no NIC receive queue's interrupt; point the \
                 queue interrupts at every serving CPU or leave steering on `auto`"
            ),
            #[cfg(target_os = "linux")]
            Self::Pin { cpu, error } => {
                write!(f, "cannot pin a serving thread to CPU {cpu}: {error}")
            }
        }
    }
}

impl std::error::Error for SteeringError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Affinity(error) | Self::Layout(error) | Self::Pin { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// The CPU each serving thread is pinned to, when connections are steered; empty otherwise.
#[derive(Debug, Default)]
pub(super) struct Pins {
    #[cfg(target_os = "linux")]
    cpus: Vec<usize>,
}

/// Decide whether this start steers, and which CPU each thread takes.
pub(super) fn decide(
    setting: Steering,
    threads: usize,
    addr: &ListenAddr,
) -> Result<Pins, SteeringError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (threads, addr);
        match setting {
            Steering::On => Err(SteeringError::NotLinux),
            Steering::Auto | Steering::Off => Ok(Pins::default()),
        }
    }
    #[cfg(target_os = "linux")]
    linux::decide(setting, threads, addr)
}
