//! Registering an `io_uring` for NAPI busy polling (`IORING_REGISTER_NAPI`, Linux 6.9).
//!
//! A ring registered this way polls the NIC receive queues of the sockets it waits on, for up to
//! its timeout, before it sleeps for an interrupt. The native runtime registers each serving
//! thread's ring when the operator turns busy polling on, and `skyzen doctor` registers a probe
//! ring to say whether that start would succeed; both go through [`NapiPolling::register`], so
//! they cannot disagree about what the kernel's answer means.
//!
//! The ring tracks the NAPI instances of its sockets itself (`IO_URING_NAPI_TRACKING_DYNAMIC`, the
//! one mode Linux 6.9 has).

use std::{
    fmt, io,
    os::fd::{AsFd, AsRawFd, BorrowedFd},
};

use io_uring::types::Napi;

/// `IORING_REGISTER_NAPI` in `enum io_uring_register_op` (`include/uapi/linux/io_uring.h`).
const IORING_REGISTER_NAPI: libc::c_uint = 27;

/// A ring's NAPI busy-polling settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NapiPolling {
    /// How long the ring polls before it sleeps, in microseconds; 0 polls not at all.
    pub timeout_micros: u32,
    /// Whether the ring asks the kernel to defer the queues' softirq processing to its polling
    /// (`prefer_busy_poll`).
    pub prefer: bool,
}

/// Why a ring could not be registered.
#[derive(Debug)]
pub enum NapiError {
    /// The kernel has no NAPI registration: older than Linux 6.9, or built without
    /// `CONFIG_NET_RX_BUSY_POLL`. Both answer `EINVAL`, as does a ring set up with
    /// `IORING_SETUP_IOPOLL`, which nothing here creates.
    Unsupported,
    /// The kernel refused for another reason.
    Refused(io::Error),
}

impl fmt::Display for NapiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str(
                "this kernel has no io_uring NAPI registration (IORING_REGISTER_NAPI, Linux 6.9 \
                 with CONFIG_NET_RX_BUSY_POLL)",
            ),
            Self::Refused(error) => write!(f, "registering the io_uring for NAPI failed: {error}"),
        }
    }
}

impl std::error::Error for NapiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unsupported => None,
            Self::Refused(error) => Some(error),
        }
    }
}

impl NapiPolling {
    /// Register `ring` to busy-poll with these settings, and return the settings it had before.
    ///
    /// A ring created with `IORING_SETUP_SINGLE_ISSUER` must be registered from its submitting
    /// thread.
    ///
    /// # Errors
    ///
    /// [`NapiError::Unsupported`] when the kernel has no NAPI registration, else whatever else
    /// it refuses with.
    #[allow(
        unsafe_code,
        reason = "io_uring_register(2) on a ring another crate owns has no safe binding: \
                  io-uring's Submitter exists only for rings it created"
    )]
    pub fn register(self, ring: BorrowedFd<'_>) -> Result<Self, NapiError> {
        let mut napi = Napi::new()
            .set_busy_poll_timeout(self.timeout_micros)
            .set_prefer_busy_poll(self.prefer);
        // SAFETY: `ring` is an open descriptor for as long as it is borrowed, and `napi` is a
        // `struct io_uring_napi` (`Napi` is `repr(transparent)` over it) that outlives the call;
        // the kernel reads it, then writes the previous settings back into it.
        let result = unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                ring.as_raw_fd(),
                IORING_REGISTER_NAPI,
                (&raw mut napi).cast::<libc::c_void>(),
                1_u32,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(libc::EINVAL) {
                NapiError::Unsupported
            } else {
                NapiError::Refused(error)
            });
        }
        Ok(Self {
            timeout_micros: napi.busy_poll_timeout(),
            prefer: napi.prefer_busy_poll(),
        })
    }
}

/// Whether this kernel registers an `io_uring` for NAPI busy polling, found by registering a
/// probe ring with polling off.
///
/// # Errors
///
/// No ring can be set up, or the kernel refuses the registration for a reason other than not
/// having it.
pub fn napi_registration() -> io::Result<bool> {
    let ring = io_uring::IoUring::new(1)?;
    match NapiPolling::default().register(ring.as_fd()) {
        Ok(_) => Ok(true),
        Err(NapiError::Unsupported) => Ok(false),
        Err(NapiError::Refused(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use super::{napi_registration, NapiPolling};

    /// Registering hands back the ring's previous settings, so a second registration reads the
    /// first. Needs a Linux 6.9 kernel, which is the floor busy polling is offered on.
    #[test]
    fn a_registration_is_read_back_by_the_next() {
        assert!(
            napi_registration().expect("probe"),
            "the kernel has NAPI registration"
        );
        let ring = io_uring::IoUring::new(1).expect("a ring");
        let fd = ring.as_fd();
        let polling = NapiPolling {
            timeout_micros: 50,
            prefer: true,
        };
        assert_eq!(
            polling.register(fd).expect("register"),
            NapiPolling::default()
        );
        assert_eq!(polling.register(fd).expect("register again"), polling);
    }
}
