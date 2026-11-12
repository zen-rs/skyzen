//! The static file server's cache of resolved files.
//!
//! **Placement.** One cache per mounted directory per serving thread, owned by the router that
//! thread built (AGENTS.md §2a). A hit is a hash lookup and an `Rc` clone with no lock and no
//! cross-core traffic; a shared cache would put a lock or a channel round-trip on every request,
//! which costs about what the `statx` it saves did. The price is that each thread fills its own
//! copy, so memory is the operator's per-thread budget times the number of serving threads.
//!
//! **Invalidation.** Each cache owns a [`notify`] watcher on its directory. The watcher's thread
//! bumps a generation counter on any event other than an access; the cache compares it on every
//! lookup and drops everything when it moved. Dropping the whole cache rather than the touched
//! paths is deliberate: a change can re-resolve paths it does not name — a new `index.html`, an
//! SPA fallback, a sibling `.br` — and a static tree changes at deploy time, not per request. A
//! file loaded while the counter moves is served once and not cached, so a load that raced a
//! change cannot outlive it. A watcher error, an overflowed kernel queue included, is treated as a
//! change.
//!
//! **Settings.** [`StaticCacheSettings`] holds the operator's two numbers. It is a shared handle:
//! every clone, on every thread, reads the same values, and a change applies to every cache at
//! its next lookup — the cache is emptied and rebuilt at the new size.

use std::{
    cell::{Cell, RefCell},
    num::NonZeroUsize,
    path::Path,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};

use lru::LruCache;
use notify::{EventKind, RecursiveMode, Watcher as _};

/// The operator's settings for every [`StaticDir`](super::StaticDir) cache that shares this
/// handle, adjustable while the server runs.
///
/// Clone it into each `StaticDir` (it is `Send + Sync`, so one handle built outside the router
/// factory reaches every serving thread), and keep a clone wherever the operator's controls live.
///
/// ```rust
/// # #[cfg(not(target_arch = "wasm32"))] {
/// use skyzen::{
///     runtime::ServingThread,
///     static_files::{StaticCacheSettings, StaticDir},
/// };
///
/// let settings = StaticCacheSettings::new(4096, 256 * 1024);
/// // On each serving thread, inside the endpoint factory.
/// let assets = |thread: &ServingThread| StaticDir::new("/assets", "./dist", thread).cache(settings.clone());
/// // Later, from an admin endpoint or a configuration reload:
/// settings.set_entries(16_384);
/// # let _ = assets;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct StaticCacheSettings(Arc<Settings>);

#[derive(Debug)]
struct Settings {
    entries: AtomicUsize,
    inline_limit: AtomicU64,
    /// Bumped by every change, so a cache notices one with a single load.
    revision: AtomicU64,
}

impl StaticCacheSettings {
    /// Cache at most `entries` resolved files per thread, and hold a file in memory when it is
    /// smaller than `inline_limit` bytes. A larger file is cached as an open descriptor and sent
    /// as a file region. `entries` of zero turns the cache off.
    #[must_use]
    pub fn new(entries: usize, inline_limit: u64) -> Self {
        Self(Arc::new(Settings {
            entries: AtomicUsize::new(entries),
            inline_limit: AtomicU64::new(inline_limit),
            revision: AtomicU64::new(0),
        }))
    }

    /// How many resolved files each thread's cache holds.
    #[must_use]
    pub fn entries(&self) -> usize {
        self.0.entries.load(Ordering::Relaxed)
    }

    /// Files smaller than this many bytes are held in memory.
    #[must_use]
    pub fn inline_limit(&self) -> u64 {
        self.0.inline_limit.load(Ordering::Relaxed)
    }

    /// Change how many resolved files each thread's cache holds; zero turns caching off.
    pub fn set_entries(&self, entries: usize) {
        self.0.entries.store(entries, Ordering::Relaxed);
        self.0.revision.fetch_add(1, Ordering::Release);
    }

    /// Change the size below which a file is held in memory.
    pub fn set_inline_limit(&self, bytes: u64) {
        self.0.inline_limit.store(bytes, Ordering::Relaxed);
        self.0.revision.fetch_add(1, Ordering::Release);
    }

    fn revision(&self) -> u64 {
        self.0.revision.load(Ordering::Acquire)
    }
}

impl Default for StaticCacheSettings {
    /// 1024 files per thread, held in memory below 64 KiB: a typical bundle's scripts, styles and
    /// icons stay in memory, and media is sent from the file.
    fn default() -> Self {
        Self::new(1024, 64 * 1024)
    }
}

/// A point in the watcher's history, taken before a load so the insert can tell whether the
/// directory changed while the load ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Generation(u64);

/// One thread's cache for one mounted directory.
#[derive(Debug)]
pub(super) struct DirCache<V> {
    settings: StaticCacheSettings,
    seen_revision: Cell<u64>,
    inline_limit: Cell<u64>,
    changes: Arc<AtomicU64>,
    seen_changes: Cell<u64>,
    entries: RefCell<Option<LruCache<Box<str>, Rc<V>>>>,
    _watcher: notify::RecommendedWatcher,
}

impl<V> DirCache<V> {
    /// A cache for `directory`, watching it from now on.
    ///
    /// # Panics
    ///
    /// When the directory cannot be watched — it does not exist, or the kernel's watch limit is
    /// reached. Serving it uncached would hide that, and caching it unwatched would serve stale
    /// files, so the router does not start.
    pub(super) fn new(directory: &Path, settings: StaticCacheSettings) -> Self {
        let changes = Arc::new(AtomicU64::new(0));
        let mut watcher = {
            let changes = Arc::clone(&changes);
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                match event {
                    Ok(event) if matches!(event.kind, EventKind::Access(_)) => return,
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%error, "static file watcher error; dropping the cache");
                    }
                }
                changes.fetch_add(1, Ordering::Release);
            })
        }
        .unwrap_or_else(|error| panic!("cannot create a filesystem watcher: {error}"));
        watcher
            .watch(directory, RecursiveMode::Recursive)
            .unwrap_or_else(|error| {
                panic!(
                    "cannot watch {} for changes, so its cache could serve stale files: {error}",
                    directory.display()
                )
            });

        let revision = settings.revision();
        let inline_limit = settings.inline_limit();
        let entries = RefCell::new(lru_with(settings.entries()));
        Self {
            settings,
            seen_revision: Cell::new(revision),
            inline_limit: Cell::new(inline_limit),
            changes,
            seen_changes: Cell::new(0),
            entries,
            _watcher: watcher,
        }
    }

    /// The size below which a file loaded now is held in memory.
    pub(super) const fn inline_limit(&self) -> u64 {
        self.inline_limit.get()
    }

    /// The cached value for `key`, after applying any settings change or filesystem event.
    pub(super) fn get(&self, key: &str) -> Option<Rc<V>> {
        self.refresh();
        self.entries
            .borrow_mut()
            .as_mut()
            .and_then(|entries| entries.get(key).cloned())
    }

    /// Where the watcher's history stands, for a load about to start.
    pub(super) fn generation(&self) -> Generation {
        Generation(self.changes.load(Ordering::Acquire))
    }

    /// Cache `value` under `key`, unless the directory changed since `generation` was taken.
    pub(super) fn insert(&self, key: &str, value: Rc<V>, generation: Generation) {
        self.refresh();
        if self.generation() != generation {
            return;
        }
        if let Some(entries) = self.entries.borrow_mut().as_mut() {
            entries.put(key.into(), value);
        }
    }

    fn refresh(&self) {
        let revision = self.settings.revision();
        if revision != self.seen_revision.get() {
            self.seen_revision.set(revision);
            self.inline_limit.set(self.settings.inline_limit());
            *self.entries.borrow_mut() = lru_with(self.settings.entries());
        }
        let changes = self.changes.load(Ordering::Acquire);
        if changes != self.seen_changes.get() {
            self.seen_changes.set(changes);
            if let Some(entries) = self.entries.borrow_mut().as_mut() {
                entries.clear();
            }
        }
    }
}

fn lru_with<V>(entries: usize) -> Option<LruCache<Box<str>, Rc<V>>> {
    NonZeroUsize::new(entries).map(LruCache::new)
}
