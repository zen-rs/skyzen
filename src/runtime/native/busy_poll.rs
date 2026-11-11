//! Busy polling the NIC receive queues instead of waiting for their interrupts (Linux).
//!
//! Off unless the operator turns it on with `--busy-poll <µs>` (`SKYZEN_BUSY_POLL`). Then each
//! serving thread registers its `io_uring` for NAPI busy polling (`IORING_REGISTER_NAPI`, Linux
//! 6.9): a wait for completions first polls the receive queues of the sockets the ring waits on,
//! for up to the timeout, before it sleeps. `SO_BUSY_POLL` alone would not do it, since the kernel
//! polls only once for a non-blocking receive, and every receive on the ring is one. The ring
//! tracks the queues of its sockets itself (`IO_URING_NAPI_TRACKING_DYNAMIC`, the one mode Linux
//! 6.9 has).
//!
//! Every serving socket also carries `SO_BUSY_POLL`, `SO_PREFER_BUSY_POLL` and
//! `SO_BUSY_POLL_BUDGET`, set on each TCP listener before it listens and on each QUIC socket before
//! it binds. A connection inherits the three from its listener: `sk_clone_lock` copies every field
//! of `struct sock` outside `sk_dontcopy_begin..sk_dontcopy_end`, and the three are outside it
//! (`net/core/sock.c`, `include/net/sock.h`).
//!
//! The settings are fixed for the life of the process: a ring set up with
//! `IORING_SETUP_SINGLE_ISSUER` takes registrations only from its own thread.
//!
//! Asking for busy polling where it cannot be had is a refusal to start: off Linux, on a unix
//! socket, under a serverless host, on a runtime that fell back from `io_uring` to `epoll`, and on
//! a kernel without NAPI registration. `skyzen doctor` reports the last through the same
//! registration, `skyzen_core::busy_poll`.

use core::{
    fmt,
    num::{NonZeroU16, NonZeroU32},
    str::FromStr,
};

use super::ListenAddr;

/// How long a wait busy-polls before it sleeps, in microseconds: at least 1, and at most
/// `i32::MAX`, since `SO_BUSY_POLL` takes an `int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusyPollTimeout(NonZeroU32);

impl BusyPollTimeout {
    /// The timeout of `micros` microseconds, or `None` when it is 0 or above `i32::MAX`.
    #[must_use]
    pub fn from_micros(micros: u32) -> Option<Self> {
        i32::try_from(micros).ok()?;
        NonZeroU32::new(micros).map(Self)
    }

    /// The timeout in microseconds.
    #[must_use]
    pub const fn micros(self) -> u32 {
        self.0.get()
    }
}

impl FromStr for BusyPollTimeout {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let micros: u32 = value.parse().map_err(|error| format!("{error}"))?;
        Self::from_micros(micros).ok_or_else(|| {
            format!(
                "a busy-poll timeout is between 1 and {} microseconds",
                i32::MAX
            )
        })
    }
}

/// The operator's busy-polling settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusyPoll {
    /// How long a wait polls before it sleeps (`--busy-poll`): the ring's timeout and each
    /// socket's `SO_BUSY_POLL`.
    pub timeout: BusyPollTimeout,
    /// Whether the kernel defers the queues' softirq processing to the polling
    /// (`--busy-poll-prefer`): the ring's `prefer_busy_poll` and each socket's
    /// `SO_PREFER_BUSY_POLL`. Turning it on needs `CAP_NET_ADMIN`.
    pub prefer: bool,
    /// How many packets one poll of a socket processes (`--busy-poll-budget`), each socket's
    /// `SO_BUSY_POLL_BUDGET`; `None` is the kernel's `BUSY_POLL_BUDGET`, 8. A ring polls with that
    /// fixed budget whatever the sockets say. Raising it needs `CAP_NET_ADMIN`.
    pub budget: Option<NonZeroU16>,
}

/// Why busy polling cannot be had.
#[derive(Debug)]
pub(super) enum BusyPollError {
    /// Busy polling is Linux's.
    #[cfg(not(target_os = "linux"))]
    NotLinux,
    /// The listener is a unix socket, which no NIC queue delivers to.
    #[cfg(target_os = "linux")]
    NotTcp,
    /// This platform's host owns the sockets.
    NotServer,
    /// The serving runtime runs on another driver than `io_uring`.
    #[cfg(target_os = "linux")]
    NotIoUring(compio::driver::DriverType),
    /// The ring could not be registered.
    #[cfg(target_os = "linux")]
    Napi(skyzen_core::busy_poll::NapiError),
}

impl fmt::Display for BusyPollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const ASKED: &str = "busy polling is on (--busy-poll or SKYZEN_BUSY_POLL)";
        match self {
            #[cfg(not(target_os = "linux"))]
            Self::NotLinux => write!(f, "{ASKED}, but only Linux busy-polls"),
            #[cfg(target_os = "linux")]
            Self::NotTcp => write!(f, "{ASKED}, but the listener is a unix socket"),
            Self::NotServer => write!(
                f,
                "{ASKED}, but this platform owns the listening sockets; unset it or run where it \
                 applies"
            ),
            #[cfg(target_os = "linux")]
            Self::NotIoUring(driver) => write!(
                f,
                "{ASKED}, but the serving runtime runs on {driver:?}, not io_uring, whose ring is \
                 what busy-polls"
            ),
            #[cfg(target_os = "linux")]
            Self::Napi(error) => write!(f, "{ASKED}, but {error}"),
        }
    }
}

impl std::error::Error for BusyPollError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Napi(error) => Some(error),
            _ => None,
        }
    }
}

/// Whether a start on `addr` can busy-poll, when the operator asked for it.
pub(super) const fn check(
    busy_poll: Option<&BusyPoll>,
    addr: &ListenAddr,
) -> Result<(), BusyPollError> {
    if busy_poll.is_none() {
        return Ok(());
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = addr;
        Err(BusyPollError::NotLinux)
    }
    #[cfg(target_os = "linux")]
    match addr {
        ListenAddr::Tcp(_) => Ok(()),
        ListenAddr::Unix(_) => Err(BusyPollError::NotTcp),
    }
}

#[cfg(target_os = "linux")]
impl BusyPoll {
    /// Give `socket` the three busy-poll options.
    ///
    /// # Errors
    ///
    /// The kernel refuses one; `EPERM` names the capability it wants.
    pub(super) fn set_on(self, socket: &socket2::Socket) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;

        let options = [
            (
                "SO_BUSY_POLL",
                libc::SO_BUSY_POLL,
                i32::try_from(self.timeout.micros()).expect("a timeout fits an int, as parsed"),
            ),
            (
                "SO_PREFER_BUSY_POLL",
                libc::SO_PREFER_BUSY_POLL,
                i32::from(self.prefer),
            ),
            // 0 leaves the kernel's own budget, `BUSY_POLL_BUDGET` (`include/net/busy_poll.h`).
            (
                "SO_BUSY_POLL_BUDGET",
                libc::SO_BUSY_POLL_BUDGET,
                self.budget.map_or(0, |budget| i32::from(budget.get())),
            ),
        ];
        for (name, option, value) in options {
            // SAFETY: `value` is an `int` that outlives the call, and its size is the length
            // passed.
            let result = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&raw const value).cast(),
                    libc::socklen_t::try_from(size_of::<i32>()).expect("an int's size fits"),
                )
            };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                let hint = if error.raw_os_error() == Some(libc::EPERM) {
                    "; preferring busy polling and raising its budget need CAP_NET_ADMIN"
                } else {
                    ""
                };
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("busy polling: the kernel refused {name} = {value}: {error}{hint}"),
                ));
            }
        }
        Ok(())
    }

    /// Register the calling thread's `runtime`, which must be `io_uring`'s, to busy-poll.
    ///
    /// # Errors
    ///
    /// The runtime is not `io_uring`'s, or the kernel refuses the registration.
    pub(super) fn register(
        self,
        runtime: &executor_core::compio::Runtime,
    ) -> Result<(), BusyPollError> {
        use std::os::fd::{AsRawFd, BorrowedFd};

        use compio::driver::DriverType;
        use skyzen_core::busy_poll::NapiPolling;

        let driver = runtime.driver_type();
        if driver != DriverType::IoUring {
            return Err(BusyPollError::NotIoUring(driver));
        }
        // SAFETY: the descriptor is the ring's, open for as long as `runtime` is borrowed.
        let ring = unsafe { BorrowedFd::borrow_raw(runtime.as_raw_fd()) };
        NapiPolling {
            timeout_micros: self.timeout.micros(),
            prefer: self.prefer,
        }
        .register(ring)
        .map(drop)
        .map_err(BusyPollError::Napi)
    }
}

/// The `SO_BUSY_POLL` and `SO_PREFER_BUSY_POLL` a socket carries. `SO_BUSY_POLL_BUDGET` has no
/// `getsockopt` side (`sk_getsockopt` in `net/core/sock.c`), so it cannot be read back.
#[cfg(all(test, target_os = "linux"))]
pub(super) fn read_socket_options(socket: &impl std::os::fd::AsFd) -> (u32, bool) {
    let fd = std::os::fd::AsRawFd::as_raw_fd(&socket.as_fd());
    let read = |option| {
        let mut value: libc::c_int = 0;
        let mut len = libc::socklen_t::try_from(size_of::<libc::c_int>()).unwrap();
        // SAFETY: `value` and `len` outlive the call and `len` is `value`'s size.
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                (&raw mut value).cast(),
                &raw mut len,
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        value
    };
    (
        u32::try_from(read(libc::SO_BUSY_POLL)).unwrap(),
        read(libc::SO_PREFER_BUSY_POLL) != 0,
    )
}

#[cfg(test)]
mod tests {
    use super::{check, BusyPoll, BusyPollError, BusyPollTimeout};
    use crate::runtime::native::ListenAddr;

    fn busy_poll() -> BusyPoll {
        BusyPoll {
            timeout: BusyPollTimeout::from_micros(50).unwrap(),
            prefer: false,
            budget: None,
        }
    }

    #[test]
    fn nothing_is_refused_when_busy_polling_is_off() {
        let tcp = ListenAddr::Tcp("127.0.0.1:0".parse().unwrap());
        assert!(check(None, &tcp).is_ok());
    }

    /// Only Linux busy-polls, and a start that asks for it elsewhere refuses.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn busy_polling_off_linux_is_refused() {
        let tcp = ListenAddr::Tcp("127.0.0.1:0".parse().unwrap());
        let error = check(Some(&busy_poll()), &tcp).expect_err("not Linux");
        assert!(matches!(error, BusyPollError::NotLinux), "{error}");
        assert!(error.to_string().contains("only Linux"), "{error}");
    }

    /// A unix socket has no NIC queue to poll.
    #[cfg(target_os = "linux")]
    #[test]
    fn busy_polling_a_unix_socket_is_refused() {
        let tcp = ListenAddr::Tcp("127.0.0.1:0".parse().unwrap());
        assert!(check(Some(&busy_poll()), &tcp).is_ok());
        let unix = ListenAddr::Unix("/run/app.sock".into());
        let error = check(Some(&busy_poll()), &unix).expect_err("a unix socket");
        assert!(matches!(error, BusyPollError::NotTcp), "{error}");
    }

    #[test]
    fn a_timeout_is_a_positive_int() {
        assert_eq!("50".parse::<BusyPollTimeout>().unwrap().micros(), 50);
        assert!("0".parse::<BusyPollTimeout>().is_err());
        assert!("2147483648".parse::<BusyPollTimeout>().is_err());
        assert!("-1".parse::<BusyPollTimeout>().is_err());
        assert_eq!(
            BusyPollTimeout::from_micros(2_147_483_647).map(BusyPollTimeout::micros),
            Some(2_147_483_647)
        );
    }
}
