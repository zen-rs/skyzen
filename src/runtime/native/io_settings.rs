//! The operator's settings for the serving threads' I/O: the provided buffer ring each thread's
//! compio runtime receives into, the size from which a write is sent zero-copy, the size of
//! each ring's registered file table, and busy polling.
//!
//! Each is set like every other startup setting of the native runtime, by a flag that beats an
//! environment variable (`--steering`/`SKYZEN_STEERING`):
//!
//! | Flag | Variable | Default |
//! |---|---|---|
//! | `--recv-buffers <count>` | `SKYZEN_RECV_BUFFERS` | the thread's share of the connection limit |
//! | `--recv-buffer-len <bytes>` | `SKYZEN_RECV_BUFFER_LEN` | compio's, 8 KiB |
//! | `--zero-copy-threshold <bytes>` | `SKYZEN_ZERO_COPY_THRESHOLD` | [`ZeroCopySend::DEFAULT_THRESHOLD`] |
//! | `--file-table-slots <count>` | `SKYZEN_FILE_TABLE_SLOTS` | [`FileTable::DEFAULT_SLOTS`] |
//! | `--busy-poll <µs>` | `SKYZEN_BUSY_POLL` | off |
//! | `--busy-poll-prefer <true\|false>` | `SKYZEN_BUSY_POLL_PREFER` | `false` |
//! | `--busy-poll-budget <packets>` | `SKYZEN_BUSY_POLL_BUDGET` | the kernel's, 8 |
//!
//! Busy polling (see [`BusyPoll`]) is on exactly when `--busy-poll` sets its timeout; the other
//! two refine it, and setting either without it stops startup. It is fixed for the life of the
//! process: each thread registers its ring when it builds its runtime, and the sockets take their
//! options when they are bound.
//!
//! The ring is fixed when a thread builds its runtime: compio registers it with the kernel then,
//! and a provided buffer ring cannot be resized. The ring is registered for incremental
//! consumption (`IOU_PBUF_RING_INC`): a receive takes only the bytes it needs from a buffer and
//! the next continues in it, so many small reads share one buffer. Kernels before Linux 6.12
//! reject that, and compio's fork then registers the ring consuming whole buffers. The other two are shared handles, like
//! [`StaticCacheSettings`](crate::static_files::StaticCacheSettings): every serving thread's
//! driver holds a clone of the same two, and a handler that extracts [`IoSettings`] changes them
//! for every thread at once. A new zero-copy threshold applies to the next write of every
//! connection; a new file-table size applies to rings that register their table afterwards, which
//! a serving thread does when its first connection arrives, so on a running server only `0` (no
//! slots for connections created afterwards) still changes anything.
//!
//! All of it matters on Linux over `io_uring` only; elsewhere the values are accepted and unused,
//! except busy polling, which refuses to start where it cannot be had.

use core::{
    future::{ready, Future},
    num::{NonZeroU16, NonZeroUsize},
    str::FromStr,
};
use std::fmt::Display;

use http_kit::http_error;
use http_kit_io::compio::{Compio, FileTable, ZeroCopySend};
use skyzen_core::Extractor;
use tracing::info;

use super::{busy_poll::BusyPoll, RuntimeFailure};
use crate::StatusCode;

/// The I/O settings the flags and environment asked for, before the runtime starts.
///
/// `None` is the default for that setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoOptions {
    /// How many buffers each serving thread's receive ring holds (`--recv-buffers`).
    pub recv_buffers: Option<NonZeroU16>,
    /// How many bytes each of those buffers holds (`--recv-buffer-len`).
    pub recv_buffer_len: Option<NonZeroUsize>,
    /// The fewest bytes a write sends zero-copy (`--zero-copy-threshold`).
    pub zero_copy_threshold: Option<usize>,
    /// How many sockets each ring's file table holds (`--file-table-slots`).
    pub file_table_slots: Option<u32>,
    /// Busy polling, when `--busy-poll` turns it on; `None` is off.
    pub busy_poll: Option<BusyPoll>,
}

/// compio's own buffer length (`ProactorBuilder`'s `buffer_pool_buffer_len` default), which the
/// ring keeps unless the operator sets one.
const DEFAULT_RECV_BUFFER_LEN: NonZeroUsize = NonZeroUsize::new(8 * 1024).unwrap();

impl IoOptions {
    /// The driver every serving thread runs its sockets on, holding the two shared handles these
    /// options start at.
    #[must_use]
    pub(crate) fn driver(&self) -> Compio {
        Compio::default()
            .with_zero_copy(ZeroCopySend::new(
                self.zero_copy_threshold
                    .unwrap_or(ZeroCopySend::DEFAULT_THRESHOLD),
            ))
            .with_file_table(FileTable::new(
                self.file_table_slots.unwrap_or(FileTable::DEFAULT_SLOTS),
            ))
    }

    /// The receive ring of a thread that keeps up to `connections` open.
    ///
    /// Unless the operator sized it, the ring holds one buffer per connection the thread may keep
    /// open, so every open connection can have an arrival waiting to be read before a receive has
    /// to fall back to a buffer of its own (compio's default of 8 runs out at the ninth).
    pub(crate) fn recv_ring(&self, connections: NonZeroUsize) -> RecvRing {
        let buffers = self.recv_buffers.unwrap_or_else(|| {
            let connections = u16::try_from(connections.get()).expect(
                "a thread's share of the connection limit fits a ring, whose count is a u16",
            );
            NonZeroU16::new(connections).expect("the share is not zero")
        });
        RecvRing {
            buffers,
            buffer_len: self.recv_buffer_len.unwrap_or(DEFAULT_RECV_BUFFER_LEN),
        }
    }
}

/// Parse the I/O settings from the command line, falling back per setting to its environment
/// variable; see the module documentation for the names.
///
/// # Panics
///
/// A value that does not parse kills the process at startup, like every other malformed setting,
/// as does a busy-poll preference or budget without `--busy-poll`.
#[must_use]
pub fn apply_io_overrides(args: impl IntoIterator<Item = String>) -> IoOptions {
    let args: Vec<String> = args.into_iter().skip(1).collect();
    let prefer = setting(&args, "--busy-poll-prefer", "SKYZEN_BUSY_POLL_PREFER");
    let budget = setting(&args, "--busy-poll-budget", "SKYZEN_BUSY_POLL_BUDGET");
    let timeout = setting(&args, "--busy-poll", "SKYZEN_BUSY_POLL");
    assert!(
        timeout.is_some() || (prefer.is_none() && budget.is_none()),
        "--busy-poll-prefer and --busy-poll-budget refine busy polling, which is off: set its \
         timeout with --busy-poll or SKYZEN_BUSY_POLL"
    );
    let busy_poll = timeout.map(|timeout| BusyPoll {
        timeout,
        prefer: prefer.unwrap_or(false),
        budget,
    });
    IoOptions {
        recv_buffers: setting(&args, "--recv-buffers", "SKYZEN_RECV_BUFFERS"),
        recv_buffer_len: setting(&args, "--recv-buffer-len", "SKYZEN_RECV_BUFFER_LEN"),
        zero_copy_threshold: setting(&args, "--zero-copy-threshold", "SKYZEN_ZERO_COPY_THRESHOLD"),
        file_table_slots: setting(&args, "--file-table-slots", "SKYZEN_FILE_TABLE_SLOTS"),
        busy_poll,
    }
}

/// The value `flag` (`--flag value` or `--flag=value`, the last one winning) or else `env` gives.
fn setting<T>(args: &[String], flag: &str, env: &str) -> Option<T>
where
    T: FromStr,
    T::Err: Display,
{
    let mut value = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some(inline) = arg
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            value = Some(inline.to_owned());
        } else if arg == flag {
            value = Some(
                args.next()
                    .unwrap_or_else(|| panic!("{flag} expects a value after it"))
                    .clone(),
            );
        }
    }
    let (value, origin) = match value {
        Some(value) => (value, flag),
        None => match std::env::var(env) {
            Ok(value) => (value, env),
            Err(std::env::VarError::NotPresent) => return None,
            Err(error) => panic!("{env} is not valid text: {error}"),
        },
    };
    let parsed = value
        .parse()
        .unwrap_or_else(|error| panic!("invalid {origin} `{value}`: {error}"));
    info!("{origin} set to {value}");
    Some(parsed)
}

/// The provided buffer ring a serving thread's runtime receives into: `buffers` buffers of
/// `buffer_len` bytes each, which compio rounds up to a power of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvRing {
    buffers: NonZeroU16,
    buffer_len: NonZeroUsize,
}

impl RecvRing {
    /// How many buffers the ring holds, as asked of compio.
    #[must_use]
    pub const fn buffers(&self) -> NonZeroU16 {
        self.buffers
    }

    /// How many bytes each buffer holds.
    #[must_use]
    pub const fn buffer_len(&self) -> NonZeroUsize {
        self.buffer_len
    }
}

/// The serving threads' I/O settings, as the runtime runs them.
///
/// Extract it in a handler to read them, or to change the two that are shared live (see the
/// module documentation for which change takes effect when):
///
/// ```rust,no_run
/// use skyzen::runtime::native::IoSettings;
///
/// async fn set_zero_copy(io: IoSettings) -> &'static str {
///     io.zero_copy().set_threshold(64 * 1024);
///     "ok"
/// }
/// ```
#[derive(Debug, Clone)]
pub struct IoSettings {
    driver: Compio,
    recv_ring: RecvRing,
    busy_poll: Option<BusyPoll>,
}

http_error!(
    /// The runtime did not put its I/O settings into this request.
    pub IoSettingsNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "I/O settings are not available: they are provided by the built-in native runtime, not by \
     an embedding host."
);

impl IoSettings {
    /// The settings a runtime with `driver`, `recv_ring` and `busy_poll` serves on.
    pub(crate) const fn new(
        driver: Compio,
        recv_ring: RecvRing,
        busy_poll: Option<BusyPoll>,
    ) -> Self {
        Self {
            driver,
            recv_ring,
            busy_poll,
        }
    }

    /// The threshold from which a write is sent zero-copy, shared by every serving thread.
    #[must_use]
    pub const fn zero_copy(&self) -> &ZeroCopySend {
        self.driver.zero_copy()
    }

    /// The size of each ring's registered file table, shared by every serving thread.
    #[must_use]
    pub const fn file_table(&self) -> &FileTable {
        self.driver.file_table()
    }

    /// The receive ring every serving thread's runtime was built with.
    #[must_use]
    pub const fn recv_ring(&self) -> RecvRing {
        self.recv_ring
    }

    /// Busy polling as every serving thread's ring and socket runs it; `None` is off.
    #[must_use]
    pub const fn busy_poll(&self) -> Option<BusyPoll> {
        self.busy_poll
    }

    /// The driver a serving thread's sockets are created and adopted with, and a proxy's upstream
    /// sockets dialed with.
    pub(crate) fn driver(&self) -> Compio {
        self.driver.clone()
    }

    /// A serving thread's runtime, its buffer ring sized as [`recv_ring`](Self::recv_ring) says,
    /// and its `io_uring` registered to busy-poll when [`busy_poll`](Self::busy_poll) is on.
    ///
    /// Called on the thread that runs the runtime, which is the only one its ring takes a
    /// registration from.
    ///
    /// # Errors
    ///
    /// The runtime cannot be created, or busy polling is on and the ring cannot be registered.
    pub(super) fn runtime(&self) -> Result<executor_core::compio::Runtime, RuntimeFailure> {
        let mut proactor = compio::driver::ProactorBuilder::new();
        proactor
            .buffer_pool_size(self.recv_ring.buffers)
            .buffer_pool_buffer_len(self.recv_ring.buffer_len.get());
        // Incremental consumption: a receive fills only the part of a buffer it needs, so small
        // reads share one buffer. A kernel before 6.12 rejects the flag and compio's fork
        // registers the ring whole-buffer instead.
        #[cfg(target_os = "linux")]
        proactor.buffer_pool_flag(
            u16::try_from(io_uring::types::IOU_PBUF_RING_INC)
                .expect("`IOU_PBUF_RING_INC` (2) fits `io_uring_buf_reg`'s u16 flags"),
        );
        let runtime = compio::runtime::Runtime::builder()
            .with_proactor(proactor)
            .build()?;
        if let Some(busy_poll) = &self.busy_poll {
            #[cfg(target_os = "linux")]
            busy_poll
                .register(&runtime)
                .map_err(RuntimeFailure::BusyPoll)?;
            #[cfg(not(target_os = "linux"))]
            {
                let _ = busy_poll;
                return Err(RuntimeFailure::BusyPoll(
                    super::busy_poll::BusyPollError::NotLinux,
                ));
            }
        }
        Ok(runtime)
    }
}

impl Extractor for IoSettings {
    type Error = IoSettingsNotConfigured;

    // A synchronous clone out of the extensions, so the future is ready on creation.
    fn extract(
        request: &mut crate::Request,
    ) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or_else(IoSettingsNotConfigured::new),
        )
    }
}

#[cfg(test)]
mod tests {
    use core::num::{NonZeroU16, NonZeroUsize};

    use serial_test::serial;

    use super::{apply_io_overrides, IoOptions};
    use crate::runtime::native::{BusyPoll, BusyPollTimeout};

    fn args(list: &[&str]) -> Vec<String> {
        core::iter::once("app")
            .chain(list.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    #[serial]
    fn unset_settings_take_their_defaults() {
        let options = apply_io_overrides(args(&[]));
        assert_eq!(options, IoOptions::default());
        let ring = options.recv_ring(NonZeroUsize::new(1024).unwrap());
        assert_eq!(ring.buffers().get(), 1024);
        assert_eq!(ring.buffer_len().get(), 8 * 1024);
        let driver = options.driver();
        assert_eq!(
            driver.zero_copy().threshold(),
            http_kit_io::compio::ZeroCopySend::DEFAULT_THRESHOLD
        );
        assert_eq!(
            driver.file_table().slots(),
            http_kit_io::compio::FileTable::DEFAULT_SLOTS
        );
    }

    #[test]
    #[serial]
    fn flags_set_every_setting_in_either_form() {
        let options = apply_io_overrides(args(&[
            "--recv-buffers",
            "4096",
            "--recv-buffer-len=16384",
            "--zero-copy-threshold",
            "0",
            "--file-table-slots=65536",
            "--busy-poll",
            "50",
            "--busy-poll-prefer=true",
            "--busy-poll-budget",
            "64",
        ]));
        assert_eq!(
            options,
            IoOptions {
                recv_buffers: NonZeroU16::new(4096),
                recv_buffer_len: NonZeroUsize::new(16_384),
                zero_copy_threshold: Some(0),
                file_table_slots: Some(65_536),
                busy_poll: Some(BusyPoll {
                    timeout: BusyPollTimeout::from_micros(50).unwrap(),
                    prefer: true,
                    budget: NonZeroU16::new(64),
                }),
            }
        );
        assert_eq!(
            options
                .recv_ring(NonZeroUsize::new(8).unwrap())
                .buffers()
                .get(),
            4096
        );
    }

    #[test]
    #[serial]
    fn a_change_to_the_shared_handles_reaches_every_clone() {
        let options = IoOptions::default();
        let driver = options.driver();
        let ring = options.recv_ring(NonZeroUsize::new(64).unwrap());
        let settings = super::IoSettings::new(driver.clone(), ring, None);
        settings.zero_copy().set_threshold(usize::MAX);
        settings.file_table().set_slots(0);
        let thread = settings.driver();
        assert_eq!(thread.zero_copy().threshold(), usize::MAX);
        assert_eq!(thread.file_table().slots(), 0);
        assert_eq!(driver.zero_copy().threshold(), usize::MAX);
    }

    #[test]
    #[serial]
    #[should_panic(expected = "invalid --recv-buffers `0`")]
    fn a_ring_of_no_buffers_stops_the_process() {
        let _ = apply_io_overrides(args(&["--recv-buffers", "0"]));
    }

    #[test]
    fn a_sized_ring_builds_a_runtime() {
        let options = IoOptions {
            recv_buffers: NonZeroU16::new(256),
            ..IoOptions::default()
        };
        let settings = super::IoSettings::new(
            options.driver(),
            options.recv_ring(NonZeroUsize::new(1).unwrap()),
            None,
        );
        let runtime = settings.runtime().expect("a runtime with a sized ring");
        runtime.block_on(async {});
    }

    #[test]
    #[serial]
    #[should_panic(expected = "refine busy polling, which is off")]
    fn a_busy_poll_preference_without_busy_polling_stops_the_process() {
        let _ = apply_io_overrides(args(&["--busy-poll-prefer", "true"]));
    }

    #[test]
    #[serial]
    #[should_panic(expected = "invalid --busy-poll `0`")]
    fn a_busy_poll_timeout_of_zero_stops_the_process() {
        let _ = apply_io_overrides(args(&["--busy-poll", "0"]));
    }

    /// The ring settings a serving thread's runtime was built with, read back by registering
    /// polling off, which hands back what the ring had.
    #[cfg(target_os = "linux")]
    fn ring_polling(busy_poll: Option<BusyPoll>) -> skyzen_core::busy_poll::NapiPolling {
        use std::os::fd::{AsRawFd, BorrowedFd};

        let options = IoOptions {
            busy_poll,
            ..IoOptions::default()
        };
        let settings = super::IoSettings::new(
            options.driver(),
            options.recv_ring(NonZeroUsize::new(1).unwrap()),
            options.busy_poll,
        );
        let runtime = settings.runtime().expect("a runtime");
        // SAFETY: `runtime` outlives the borrow of its ring's descriptor.
        let ring = unsafe { BorrowedFd::borrow_raw(runtime.as_raw_fd()) };
        skyzen_core::busy_poll::NapiPolling::default()
            .register(ring)
            .expect("the kernel has NAPI registration")
    }

    /// With busy polling on, a serving thread's ring is registered with its timeout and
    /// preference, which no capability guards at the ring.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_busy_polling_runtime_registers_its_ring() {
        let polling = ring_polling(Some(BusyPoll {
            timeout: BusyPollTimeout::from_micros(75).unwrap(),
            prefer: true,
            budget: None,
        }));
        assert_eq!(
            polling,
            skyzen_core::busy_poll::NapiPolling {
                timeout_micros: 75,
                prefer: true,
            }
        );
    }

    /// With busy polling off, the ring is left as `io_uring` creates it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_runtime_without_busy_polling_leaves_its_ring_alone() {
        assert_eq!(
            ring_polling(None),
            skyzen_core::busy_poll::NapiPolling::default()
        );
    }
}
