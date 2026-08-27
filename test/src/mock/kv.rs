//! In-memory key-value store for testing.

use core::future::{ready, Future};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock, RwLockWriteGuard},
    time::{Duration, Instant},
};

use skyzen_services::kv::{KeyValueStore, KvError, KvListOptions, KvListResult};

/// Cloudflare KV refuses an `expirationTtl` below 60 seconds and silently rounds shorter ones up.
const CLOUDFLARE_MIN_TTL: Duration = Duration::from_mins(1);

#[derive(Debug, Clone)]
struct Entry {
    value: Vec<u8>,
    /// The instant at which the entry stops being visible; `None` never expires.
    expires_at: Option<Instant>,
}

impl Entry {
    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|expires_at| now >= expires_at)
    }
}

/// An in-memory key-value store backed by a `HashMap`.
///
/// Each instance starts empty and is completely isolated.
/// Designed for use in tests where each test gets a fresh instance.
///
/// Supports TTL via [`KeyValueStore::put_with_ttl`]: expired keys are absent
/// from `get`, `exists` and `list` and are physically removed on the next mutation or
/// listing. [`list`](KeyValueStore::list) returns keys in lexicographic order and paginates with a
/// real cursor.
///
/// The atomic primitives ([`put_if_absent`](KeyValueStore::put_if_absent),
/// [`compare_and_swap`](KeyValueStore::compare_and_swap),
/// [`increment`](KeyValueStore::increment)) are implemented for real: each runs under the store's
/// write lock, so a test can exercise a lock or a rate limiter and see the same
/// one-winner outcome a Redis or `DynamoDB` deployment would produce.
///
/// Use [`with_min_ttl`](Self::with_min_ttl) or [`strict_cloudflare`](Self::strict_cloudflare) to
/// reproduce a platform's TTL floor, and [`fail_next_with`](Self::fail_next_with) to make the next
/// operation fail, e.g. to exercise handler error paths.
#[derive(Debug, Clone)]
pub struct InMemoryKv {
    data: Arc<RwLock<HashMap<String, Entry>>>,
    fail_next: Arc<RwLock<Option<String>>>,
    /// TTLs shorter than this are rounded up, the way a platform with a minimum expiry does.
    min_ttl: Option<Duration>,
}

impl InMemoryKv {
    /// Create a new empty in-memory KV store that honours any TTL exactly.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
            fail_next: Arc::new(RwLock::new(None)),
            min_ttl: None,
        }
    }

    /// Round every TTL up to at least `min_ttl`, the way backends with a minimum expiry do.
    ///
    /// Without this the mock honours whatever `Duration` it is handed, which is *more* permissive
    /// than production: a test that stores a 5-second nonce and asserts it is gone passes here
    /// while Cloudflare KV keeps that nonce alive for a full minute.
    #[must_use]
    pub const fn with_min_ttl(mut self, min_ttl: Duration) -> Self {
        self.min_ttl = Some(min_ttl);
        self
    }

    /// A store with Cloudflare KV's 60-second TTL floor.
    ///
    /// Use this when the code under test relies on a short expiry and will be deployed to
    /// Cloudflare, so the divergence fails the test instead of production.
    #[must_use]
    pub fn strict_cloudflare() -> Self {
        Self::new().with_min_ttl(CLOUDFLARE_MIN_TTL)
    }

    /// Make exactly the next store operation fail with
    /// [`KvError::Backend`] carrying `message`.
    ///
    /// Subsequent operations succeed again. Useful for testing how handlers
    /// react to backend failures (typically a 500 response).
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn fail_next_with(&self, message: &str) {
        *self.fail_next.write().expect("InMemoryKv lock poisoned") = Some(message.to_owned());
    }

    fn take_injected_failure(&self) -> Result<(), KvError> {
        let mut slot = self.fail_next.write().expect("InMemoryKv lock poisoned");
        slot.take().map_or(Ok(()), |message| {
            drop(slot);
            Err(KvError::backend(message))
        })
    }

    /// Take the write lock, dropping every entry that has already expired.
    ///
    /// Every mutation goes through here, so the atomic primitives observe the same "expired means
    /// absent" view that `get` does while holding the lock that makes them atomic.
    fn write_purged(&self) -> RwLockWriteGuard<'_, HashMap<String, Entry>> {
        let now = Instant::now();
        let mut data = self.data.write().expect("InMemoryKv lock poisoned");
        data.retain(|_, entry| !entry.is_expired(now));
        data
    }

    /// The expiry instant for `ttl`, applying the configured floor.
    ///
    /// A TTL too large to represent never expires.
    fn expiry_for(&self, ttl: Duration) -> Option<Instant> {
        let ttl = self.min_ttl.map_or(ttl, |min_ttl| ttl.max(min_ttl));
        Instant::now().checked_add(ttl)
    }
}

impl Default for InMemoryKv {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a stored counter, treating an absent key as zero.
///
/// A value that is not a decimal integer is a caller mistake — `INCR` on a non-numeric key fails
/// on Redis too — so it is reported rather than silently reset to zero.
fn counter_value(entry: Option<&Entry>) -> Result<i64, KvError> {
    let Some(entry) = entry else {
        return Ok(0);
    };
    let text = core::str::from_utf8(&entry.value)
        .map_err(|error| KvError::Decode(format!("counter is not valid UTF-8: {error}")))?;
    text.parse()
        .map_err(|error| KvError::Decode(format!("counter {text:?} is not an integer: {error}")))
}

// A `HashMap` behind a lock answers every call synchronously, so each future is ready on creation
// rather than an `async` block with nothing to await.
impl KeyValueStore for InMemoryKv {
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, KvError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let now = Instant::now();
            let data = self.data.read().expect("InMemoryKv lock poisoned");
            data.get(key)
                .filter(|entry| !entry.is_expired(now))
                .map(|entry| entry.value.clone())
        }))
    }

    fn put(&self, key: &str, value: &[u8]) -> impl Future<Output = Result<(), KvError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            self.write_purged().insert(
                key.to_owned(),
                Entry {
                    value: value.to_vec(),
                    expires_at: None,
                },
            );
        }))
    }

    fn put_with_ttl(
        &self,
        key: &str,
        value: &[u8],
        ttl: Duration,
    ) -> impl Future<Output = Result<(), KvError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let expires_at = self.expiry_for(ttl);
            self.write_purged().insert(
                key.to_owned(),
                Entry {
                    value: value.to_vec(),
                    expires_at,
                },
            );
        }))
    }

    fn put_if_absent(
        &self,
        key: &str,
        value: &[u8],
    ) -> impl Future<Output = Result<bool, KvError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let mut data = self.write_purged();
            !data.contains_key(key)
                && data
                    .insert(
                        key.to_owned(),
                        Entry {
                            value: value.to_vec(),
                            expires_at: None,
                        },
                    )
                    .is_none()
        }))
    }

    fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: &[u8],
    ) -> impl Future<Output = Result<bool, KvError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let mut data = self.write_purged();
            data.get(key).map(|entry| entry.value.as_slice()) == expected && {
                data.insert(
                    key.to_owned(),
                    Entry {
                        value: new.to_vec(),
                        // A swap re-arms nothing: the winner owns the key outright, as `SET` does.
                        expires_at: None,
                    },
                );
                true
            }
        }))
    }

    fn increment(
        &self,
        key: &str,
        delta: i64,
    ) -> impl Future<Output = Result<i64, KvError>> + Send {
        ready(self.take_injected_failure().and_then(|()| {
            let mut data = self.write_purged();
            let updated = counter_value(data.get(key))?
                .checked_add(delta)
                .ok_or_else(|| KvError::Decode(format!("counter {key:?} overflowed an i64")))?;
            // Keep whatever expiry the counter already had, so an incremented rate-limit
            // window still closes on schedule.
            let expires_at = data.get(key).and_then(|entry| entry.expires_at);
            data.insert(
                key.to_owned(),
                Entry {
                    value: updated.to_string().into_bytes(),
                    expires_at,
                },
            );
            drop(data);
            Ok(updated)
        }))
    }

    fn expire(
        &self,
        key: &str,
        ttl: Duration,
    ) -> impl Future<Output = Result<bool, KvError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let expires_at = self.expiry_for(ttl);
            let mut data = self.write_purged();
            data.get_mut(key).is_some_and(|entry| {
                entry.expires_at = expires_at;
                true
            })
        }))
    }

    fn exists(&self, key: &str) -> impl Future<Output = Result<bool, KvError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let now = Instant::now();
            let data = self.data.read().expect("InMemoryKv lock poisoned");
            data.get(key).is_some_and(|entry| !entry.is_expired(now))
        }))
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<(), KvError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            self.write_purged().remove(key);
        }))
    }

    fn list(
        &self,
        options: KvListOptions,
    ) -> impl Future<Output = Result<KvListResult, KvError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let mut keys: Vec<String> = {
                let data = self.write_purged();
                data.keys()
                    .filter(|key| {
                        options
                            .prefix
                            .as_ref()
                            .is_none_or(|prefix| key.starts_with(prefix))
                            // The cursor is the last key of the previous page; resume strictly
                            // after it.
                            && options
                                .cursor
                                .as_ref()
                                .is_none_or(|cursor| key.as_str() > cursor.as_str())
                    })
                    .cloned()
                    .collect()
            };
            keys.sort_unstable();

            let mut cursor = None;
            if let Some(limit) = options.limit {
                if keys.len() > limit {
                    keys.truncate(limit);
                    cursor = keys.last().cloned();
                }
            }

            KvListResult { keys, cursor }
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use skyzen_services::{
        kv::{KeyValueStore, KvError, KvListOptions},
        Kv,
    };

    use super::InMemoryKv;

    #[tokio::test]
    async fn list_returns_keys_in_lexicographic_order() {
        let kv = InMemoryKv::new();
        for key in ["b", "c", "a", "aa"] {
            kv.put(key, b"1").await.unwrap();
        }

        assert_eq!(
            kv.list(KvListOptions::new()).await.unwrap().keys,
            vec!["a", "aa", "b", "c"]
        );
        assert_eq!(
            kv.list(KvListOptions::new().with_prefix("a"))
                .await
                .unwrap()
                .keys,
            vec!["a", "aa"]
        );
    }

    #[tokio::test]
    async fn list_paginates_with_a_cursor_across_three_pages() {
        let kv = InMemoryKv::new();
        for key in ["a", "b", "c", "d", "e"] {
            kv.put(key, b"1").await.unwrap();
        }

        let mut pages = Vec::new();
        let mut cursor = None;
        loop {
            let mut options = KvListOptions::new().with_limit(2);
            options.cursor = cursor.take();
            let page = kv.list(options).await.unwrap();
            pages.push(page.keys);
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert_eq!(
            pages,
            vec![
                vec!["a".to_owned(), "b".to_owned()],
                vec!["c".to_owned(), "d".to_owned()],
                vec!["e".to_owned()],
            ]
        );
    }

    #[tokio::test]
    async fn zero_ttl_expires_immediately_for_get_exists_and_list() {
        let kv = InMemoryKv::new();
        kv.put("keep", b"forever").await.unwrap();
        kv.put_with_ttl("gone", b"soon", Duration::ZERO)
            .await
            .unwrap();

        assert_eq!(kv.get("gone").await.unwrap(), None);
        assert!(!kv.exists("gone").await.unwrap());
        assert_eq!(
            kv.list(KvListOptions::new()).await.unwrap().keys,
            vec!["keep"]
        );
        // The expired entry is physically removed by the list purge.
        assert_eq!(kv.data.read().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn positive_ttl_keeps_the_value_visible_until_expiry() {
        let kv = InMemoryKv::new();
        kv.put_with_ttl("session", b"data", Duration::from_hours(1))
            .await
            .unwrap();

        assert_eq!(kv.get("session").await.unwrap(), Some(b"data".to_vec()));
        assert!(kv.exists("session").await.unwrap());
    }

    #[tokio::test]
    async fn overwriting_with_plain_put_clears_the_ttl() {
        let kv = InMemoryKv::new();
        kv.put_with_ttl("key", b"ttl", Duration::ZERO)
            .await
            .unwrap();
        kv.put("key", b"forever").await.unwrap();

        assert_eq!(kv.get("key").await.unwrap(), Some(b"forever".to_vec()));
    }

    #[tokio::test]
    async fn strict_cloudflare_rounds_a_short_ttl_up_to_the_platform_floor() {
        let kv = InMemoryKv::strict_cloudflare();
        kv.put_with_ttl("nonce", b"single-use", Duration::from_secs(5))
            .await
            .unwrap();

        // Cloudflare KV would keep this alive for a full minute, so the mock must too.
        assert_eq!(kv.get("nonce").await.unwrap(), Some(b"single-use".to_vec()));

        // The permissive default is what makes the divergence invisible.
        let permissive = InMemoryKv::new();
        permissive
            .put_with_ttl("nonce", b"single-use", Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(permissive.get("nonce").await.unwrap(), None);
    }

    #[tokio::test]
    async fn put_if_absent_lets_exactly_one_writer_win() {
        let kv = InMemoryKv::new();

        assert!(kv.put_if_absent("lock", b"first").await.unwrap());
        assert!(!kv.put_if_absent("lock", b"second").await.unwrap());
        assert_eq!(kv.get("lock").await.unwrap(), Some(b"first".to_vec()));

        // Releasing the lock lets the next writer take it.
        kv.delete("lock").await.unwrap();
        assert!(kv.put_if_absent("lock", b"second").await.unwrap());
    }

    #[tokio::test]
    async fn compare_and_swap_reports_a_lost_race_as_ok_false() {
        let kv = InMemoryKv::new();

        assert!(kv.compare_and_swap("doc", None, b"v1").await.unwrap());
        assert!(!kv.compare_and_swap("doc", None, b"v1-again").await.unwrap());
        assert!(kv
            .compare_and_swap("doc", Some(b"v1"), b"v2")
            .await
            .unwrap());
        assert!(!kv
            .compare_and_swap("doc", Some(b"v1"), b"v3")
            .await
            .unwrap());
        assert_eq!(kv.get("doc").await.unwrap(), Some(b"v2".to_vec()));
    }

    #[tokio::test]
    async fn increment_counts_from_zero_and_rejects_non_numeric_values() {
        let kv = InMemoryKv::new();

        assert_eq!(kv.increment("hits", 1).await.unwrap(), 1);
        assert_eq!(kv.increment("hits", 4).await.unwrap(), 5);
        assert_eq!(kv.increment("hits", -2).await.unwrap(), 3);
        assert_eq!(kv.get("hits").await.unwrap(), Some(b"3".to_vec()));

        kv.put("name", b"lexo").await.unwrap();
        assert!(matches!(
            kv.increment("name", 1).await.unwrap_err(),
            KvError::Decode(_)
        ));
    }

    #[tokio::test]
    async fn increment_keeps_the_window_a_rate_limiter_already_opened() {
        let kv = InMemoryKv::new();
        kv.put_with_ttl("window", b"1", Duration::ZERO)
            .await
            .unwrap();

        // The expired counter is purged before the increment, so the window restarts at 1
        // instead of resuming a count that should already have lapsed.
        assert_eq!(kv.increment("window", 1).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn expire_re_arms_an_existing_key_and_reports_a_missing_one() {
        let kv = InMemoryKv::new();
        kv.put("session", b"data").await.unwrap();

        assert!(kv.expire("session", Duration::from_hours(1)).await.unwrap());
        assert!(kv.exists("session").await.unwrap());

        assert!(kv.expire("session", Duration::ZERO).await.unwrap());
        assert!(!kv.exists("session").await.unwrap());

        assert!(!kv.expire("absent", Duration::from_hours(1)).await.unwrap());
    }

    #[tokio::test]
    async fn wrapper_exposes_the_atomic_primitives_through_dynamic_dispatch() {
        let kv = Kv::new(InMemoryKv::new());

        assert!(kv.put_if_absent("lock", b"held").await.unwrap());
        assert!(!kv.put_if_absent("lock", b"stolen").await.unwrap());
        assert_eq!(kv.increment("hits", 3).await.unwrap(), 3);
        assert!(kv
            .compare_and_swap("lock", Some(b"held"), b"renewed")
            .await
            .unwrap());
        assert!(kv.exists("lock").await.unwrap());
        assert_eq!(kv.list_all(None).await.unwrap(), vec!["hits", "lock"]);
    }

    #[tokio::test]
    async fn fail_next_with_fails_exactly_one_operation() {
        let kv = InMemoryKv::new();
        kv.put("key", b"value").await.unwrap();
        kv.fail_next_with("redis is down");

        let error = kv.get("key").await.unwrap_err();
        assert!(matches!(&error, KvError::Backend { message, .. } if message == "redis is down"));

        // The failure is consumed; the store works again.
        assert_eq!(kv.get("key").await.unwrap(), Some(b"value".to_vec()));
    }
}
