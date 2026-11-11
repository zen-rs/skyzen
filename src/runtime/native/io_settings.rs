//! The operator's settings for the serving threads' I/O: the provided buffer ring each thread's
//! compio runtime receives into, the size from which a write is sent zero-copy, and the size of
//! each ring's registered file table.
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
//!
//! The ring is fixed when a thread builds its runtime: compio registers it with the kernel then,
//! and a provided buffer ring cannot be resized. The other two are shared handles, like
//! [`StaticCacheSettings`](crate::static_files::StaticCacheSettings): every serving thread's
//! driver holds a clone of the same two, and a handler that extracts [`IoSettings`] changes them
//! for every thread at once. A new zero-copy threshold applies to the next write of every
//! connection; a new file-table size applies to rings that register their table afterwards, which
//! a serving thread does when its first connection arrives, so on a running server only `0` (no
//! slots for connections created afterwards) still changes anything.
//!
//! All of it matters on Linux over `io_uring` only; elsewhere the values are accepted and unused.

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
}

/// compio's own buffer length (`ProactorBuilder`'s `buffer_pool_buffer_len` default), which the
/// ring keeps unless the operator sets one.
const DEFAULT_RECV_BUFFER_LEN: NonZeroUsize = NonZeroUsize::new(8 * 1024).unwrap();

impl IoOptions {
    /// The driver every serving thread runs its sockets on, holding the two shared handles these
    /// options start at.
    #[must_use]
    pub(super) fn driver(&self) -> Compio {
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
    pub(super) fn recv_ring(&self, connections: NonZeroUsize) -> RecvRing {
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
/// A value that does not parse kills the process at startup, like every other malformed setting.
#[must_use]
pub fn apply_io_overrides(args: impl IntoIterator<Item = String>) -> IoOptions {
    let args: Vec<String> = args.into_iter().skip(1).collect();
    IoOptions {
        recv_buffers: setting(&args, "--recv-buffers", "SKYZEN_RECV_BUFFERS"),
        recv_buffer_len: setting(&args, "--recv-buffer-len", "SKYZEN_RECV_BUFFER_LEN"),
        zero_copy_threshold: setting(&args, "--zero-copy-threshold", "SKYZEN_ZERO_COPY_THRESHOLD"),
        file_table_slots: setting(&args, "--file-table-slots", "SKYZEN_FILE_TABLE_SLOTS"),
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
}

http_error!(
    /// The runtime did not put its I/O settings into this request.
    pub IoSettingsNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "I/O settings are not available: they are provided by the built-in native runtime, not by \
     an embedding host."
);

impl IoSettings {
    /// The settings a runtime with `driver` and `recv_ring` serves on.
    pub(super) const fn new(driver: Compio, recv_ring: RecvRing) -> Self {
        Self { driver, recv_ring }
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

    /// The driver a serving thread's sockets are created and adopted with.
    pub(super) fn driver(&self) -> Compio {
        self.driver.clone()
    }

    /// A serving thread's runtime, its buffer ring sized as [`recv_ring`](Self::recv_ring) says.
    ///
    /// # Errors
    ///
    /// The runtime cannot be created.
    pub(super) fn runtime(&self) -> std::io::Result<executor_core::compio::Runtime> {
        let mut proactor = compio::driver::ProactorBuilder::new();
        proactor
            .buffer_pool_size(self.recv_ring.buffers)
            .buffer_pool_buffer_len(self.recv_ring.buffer_len.get());
        compio::runtime::Runtime::builder()
            .with_proactor(proactor)
            .build()
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
        ]));
        assert_eq!(
            options,
            IoOptions {
                recv_buffers: NonZeroU16::new(4096),
                recv_buffer_len: NonZeroUsize::new(16_384),
                zero_copy_threshold: Some(0),
                file_table_slots: Some(65_536),
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
        let settings = super::IoSettings::new(driver.clone(), ring);
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
        );
        let runtime = settings.runtime().expect("a runtime with a sized ring");
        runtime.block_on(async {});
    }
}
