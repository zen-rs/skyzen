//! How [`StaticDir`](super::StaticDir) reaches the filesystem on a cache miss.
//!
//! [`CompioFs`] is the default: the serving thread's own compio runtime, so on Linux the `open` and
//! the `statx` of a path are `io_uring` operations, submitted in the same batch, and a small file's
//! read is one too, into the thread's region of registered buffers when it has one (see
//! [`StaticDir`](super::StaticDir)). [`BlockingFs`] runs the same calls on the `blocking` thread
//! pool, which belongs to no runtime, for a router served by an executor of the application's own
//! (the `hyper` embedding under Tokio).

use std::{
    fs::File,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use bytes::Bytes;

/// A filesystem backend for [`StaticDir`](super::StaticDir). Implemented by [`CompioFs`] and
/// [`BlockingFs`] only.
pub trait FileSystem: sealed::Ops + Clone + 'static {}

/// The serving thread's compio runtime: the default, and what `#[skyzen::main]` serves on.
///
/// A path's `open` and `statx` are two operations pushed before either is awaited, so on
/// `io_uring` they reach the kernel in one `io_uring_enter`. They are not *linked*: compio's
/// `OpenFile` cannot open into a fixed-file slot that a following operation could name, so nothing
/// can be chained after the open. Where compio has no `io_uring` it runs the same calls on its own
/// pool.
///
/// Every call must be made on a compio runtime; outside one compio panics, naming the problem.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompioFs;

/// The `blocking` thread pool: for a router driven by an executor other than compio.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockingFs;

impl FileSystem for CompioFs {}
impl FileSystem for BlockingFs {}

/// What a path names.
#[derive(Debug)]
pub enum Entry<H> {
    /// A directory, which is never read.
    Directory,
    /// A file, open for reading.
    File(Opened<H>),
    /// The path resolved outside the configured static root.
    Escaped,
}

/// An open file, in the backend's own handle, and what `stat` said about the same path.
#[derive(Debug)]
pub struct Opened<H> {
    pub(super) handle: H,
    pub(super) len: u64,
    pub(super) modified: Option<SystemTime>,
}

pub(super) mod sealed {
    use super::{Arc, Bytes, Entry, File, Future, PathBuf};
    use std::io;

    pub trait Ops {
        /// The backend's open file.
        type Handle;

        /// Stat `path`, and open it read-only when it is a file.
        ///
        /// When `canonical_root` is `Some`, `path` is canonicalized and rejected with
        /// [`Entry::Escaped`] if it resolves outside `canonical_root`.
        fn open(
            &self,
            path: PathBuf,
            canonical_root: Option<Arc<PathBuf>>,
        ) -> impl Future<Output = io::Result<Entry<Self::Handle>>>;

        /// Read the first `len` bytes of the file, which must still hold that many.
        fn read(&self, handle: Self::Handle, len: u64) -> impl Future<Output = io::Result<Bytes>>;

        /// Read the first `len` bytes of the file into `region`, the serving thread's region of
        /// registered buffers; `None` when the region has no room for them, or when the backend
        /// does not read on the ring the region belongs to.
        #[cfg(target_os = "linux")]
        fn read_fixed(
            &self,
            handle: &Self::Handle,
            len: u64,
            region: &http_kit_io::compio::FixedRegion,
        ) -> impl Future<Output = io::Result<Option<Bytes>>>;

        /// The file as a `std` handle, which the cache shares among its file-region bodies.
        fn std_file(&self, handle: Self::Handle) -> io::Result<File>;
    }
}

/// On Windows compio opens every file with `FILE_FLAG_OVERLAPPED`, and Windows rejects a synchronous
/// read of an overlapped handle that carries no `OVERLAPPED` (`ERROR_INVALID_PARAMETER`, os error
/// 87). A file region reads its `std` file synchronously, so that handle cannot be handed to it:
/// there the files are opened and read by `std` on the blocking pool, exactly as [`BlockingFs`] does.
#[cfg(windows)]
impl sealed::Ops for CompioFs {
    type Handle = File;

    async fn open(
        &self,
        path: PathBuf,
        canonical_root: Option<Arc<PathBuf>>,
    ) -> io::Result<Entry<Self::Handle>> {
        sealed::Ops::open(&BlockingFs, path, canonical_root).await
    }

    async fn read(&self, handle: Self::Handle, len: u64) -> io::Result<Bytes> {
        sealed::Ops::read(&BlockingFs, handle, len).await
    }

    fn std_file(&self, handle: Self::Handle) -> io::Result<File> {
        sealed::Ops::std_file(&BlockingFs, handle)
    }
}

#[cfg(not(windows))]
impl sealed::Ops for CompioFs {
    type Handle = compio::fs::File;

    async fn open(
        &self,
        path: PathBuf,
        canonical_root: Option<Arc<PathBuf>>,
    ) -> io::Result<Entry<Self::Handle>> {
        let (target, no_follow) = if let Some(root) = canonical_root {
            let Some(resolved) =
                blocking::unblock(move || resolve_within_root(&path, &root)).await?
            else {
                return Ok(Entry::Escaped);
            };
            (resolved, true)
        } else {
            (path, false)
        };

        // Both operations are pushed before either is awaited, so they leave in one submission.
        let (stat, file) = futures_util::future::join(
            compio::fs::metadata(&target),
            open_compio_file(&target, no_follow),
        )
        .await;
        let stat = stat?;
        if stat.is_dir() {
            // Opening a directory read-only succeeds on Unix and fails on Windows; either way it
            // is not needed.
            return Ok(Entry::Directory);
        }
        Ok(Entry::File(Opened {
            handle: file?,
            len: stat.len(),
            modified: stat.modified().ok(),
        }))
    }

    async fn read(&self, handle: Self::Handle, len: u64) -> io::Result<Bytes> {
        use compio::{
            buf::{BufResult, IntoInner as _, IoBuf as _},
            io::AsyncReadAtExt as _,
        };

        let len = in_memory(len)?;
        let BufResult(read, buffer) = handle
            .read_exact_at(Vec::with_capacity(len).slice(..len), 0)
            .await;
        read.map_err(shrank)?;
        Ok(Bytes::from(buffer.into_inner()))
    }

    /// Fixed reads (`IORING_OP_READ_FIXED`) on the serving thread's ring, straight into its
    /// region.
    #[cfg(target_os = "linux")]
    async fn read_fixed(
        &self,
        handle: &Self::Handle,
        len: u64,
        region: &http_kit_io::compio::FixedRegion,
    ) -> io::Result<Option<Bytes>> {
        region
            .read_at(handle.clone(), 0, in_memory(len)?)
            .await
            .map_err(shrank)
    }

    /// One `dup` of the descriptor, once per cache fill, so the cache owns a plain `File`.
    fn std_file(&self, handle: Self::Handle) -> io::Result<File> {
        let owned = std::os::fd::AsFd::as_fd(&handle).try_clone_to_owned()?;
        Ok(File::from(owned))
    }
}

impl sealed::Ops for BlockingFs {
    type Handle = File;

    async fn open(
        &self,
        path: PathBuf,
        canonical_root: Option<Arc<PathBuf>>,
    ) -> io::Result<Entry<Self::Handle>> {
        blocking::unblock(move || {
            let (target, no_follow) = if let Some(root) = canonical_root {
                let Some(resolved) = resolve_within_root(&path, &root)? else {
                    return Ok(Entry::Escaped);
                };
                (resolved, true)
            } else {
                (path, false)
            };

            // Stat first: Windows cannot open a directory as a file, and nothing needs it open.
            if std::fs::metadata(&target)?.is_dir() {
                return Ok(Entry::Directory);
            }
            let handle = open_blocking_file(&target, no_follow)?;
            let metadata = handle.metadata()?;
            Ok(Entry::File(Opened {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                handle,
            }))
        })
        .await
    }

    async fn read(&self, handle: Self::Handle, len: u64) -> io::Result<Bytes> {
        use std::io::Read as _;

        let wanted = in_memory(len)?;
        blocking::unblock(move || {
            let mut buffer = Vec::with_capacity(wanted);
            handle.take(len).read_to_end(&mut buffer)?;
            if buffer.len() < wanted {
                return Err(shrank(io::ErrorKind::UnexpectedEof.into()));
            }
            Ok(Bytes::from(buffer))
        })
        .await
    }

    /// The blocking pool reads on no ring, so it never reads into one's region.
    #[cfg(target_os = "linux")]
    fn read_fixed(
        &self,
        _: &Self::Handle,
        _: u64,
        _: &http_kit_io::compio::FixedRegion,
    ) -> impl Future<Output = io::Result<Option<Bytes>>> {
        std::future::ready(Ok(None))
    }

    fn std_file(&self, handle: Self::Handle) -> io::Result<File> {
        Ok(handle)
    }
}

fn resolve_within_root(path: &Path, root: &Path) -> io::Result<Option<PathBuf>> {
    let resolved = std::fs::canonicalize(path)?;
    if resolved.starts_with(root) {
        Ok(Some(resolved))
    } else {
        Ok(None)
    }
}

#[cfg(not(windows))]
async fn open_compio_file(path: &Path, no_follow: bool) -> io::Result<compio::fs::File> {
    #[cfg(target_os = "linux")]
    if no_follow {
        return compio::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .await;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = no_follow;
    compio::fs::File::open(path).await
}

fn open_blocking_file(path: &Path, no_follow: bool) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    if no_follow {
        use std::os::unix::fs::OpenOptionsExt as _;
        return std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = no_follow;
    File::open(path)
}

fn in_memory(len: u64) -> io::Result<usize> {
    usize::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "an inline limit larger than the address space",
        )
    })
}

/// A read that ended early: the file is shorter than its `stat` said, because it changed between
/// the two and the notification that says so has not reached the cache yet.
fn shrank(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the file shrank while it was read",
        )
    } else {
        error
    }
}
