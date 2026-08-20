//! Redis implementation of [`KeyValueStore`] for the Skyzen framework.
//!
//! This crate provides a [`Redis`] type that implements the [`KeyValueStore`] trait
//! from `skyzen-services`, enabling Redis as a key-value backend.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_redis::Redis;
//! use skyzen_services::Kv;
//!
//! let redis = Redis::connect("redis://127.0.0.1/").await?;
//! let kv = Kv::new(redis);
//! kv.put("key", b"value").await?;
//! ```
//!
//! # Feature Flags
//!
//! - `runtime-tokio` — Use Tokio as the async runtime
//! - `runtime-smol` — Use Smol as the async runtime
//!
//! At least one of them must be enabled; with neither, this crate exposes
//! nothing (the upstream `redis` crate cannot build its async support without
//! a runtime).
#![cfg(any(feature = "runtime-tokio", feature = "runtime-smol"))]

use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Client};
use skyzen_services::kv::{KeyValueStore, KvError};

/// A Redis-backed key-value store.
///
/// Wraps a [`ConnectionManager`] that automatically reconnects on connection failures.
/// Cloning is cheap — it shares the underlying connection.
#[derive(Debug, Clone)]
pub struct Redis {
    conn: ConnectionManager,
}

impl Redis {
    /// Connect to Redis using a URL (e.g. `redis://127.0.0.1/`).
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Backend`] if the connection cannot be established.
    pub async fn connect(url: &str) -> Result<Self, KvError> {
        let client = Client::open(url).map_err(|e| KvError::Backend(e.to_string()))?;
        let conn = client
            .get_connection_manager()
            .await
            .map_err(|e| KvError::Backend(e.to_string()))?;
        Ok(Self { conn })
    }

    /// Create a `Redis` instance from an existing [`ConnectionManager`].
    #[must_use]
    pub const fn from_connection_manager(conn: ConnectionManager) -> Self {
        Self { conn }
    }
}

/// Build a `SCAN MATCH` pattern that matches exactly the keys starting with `prefix`.
///
/// Redis glob metacharacters (`*`, `?`, `[`, `]`, `\`) occurring in the prefix
/// are escaped with a backslash so they match literally.
fn scan_pattern(prefix: Option<&str>) -> String {
    prefix.map_or_else(
        || "*".to_owned(),
        |p| {
            let mut pattern = String::with_capacity(p.len() + 1);
            for c in p.chars() {
                if matches!(c, '*' | '?' | '[' | ']' | '\\') {
                    pattern.push('\\');
                }
                pattern.push(c);
            }
            pattern.push('*');
            pattern
        },
    )
}

/// Convert a TTL duration to whole milliseconds for `PSETEX`/`SET PX`.
///
/// Sub-millisecond durations are rounded up to 1 ms. Zero and overflowing
/// durations are rejected because Redis requires a positive expiry.
fn ttl_millis(ttl: core::time::Duration) -> Result<u64, KvError> {
    if ttl.is_zero() {
        return Err(KvError::Backend("TTL must be greater than zero".to_owned()));
    }
    let millis = ttl.as_millis().max(1);
    u64::try_from(millis)
        .map_err(|_| KvError::Backend(format!("TTL of {millis} ms exceeds the supported maximum")))
}

impl KeyValueStore for Redis {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let mut conn = self.conn.clone();
        conn.get(key)
            .await
            .map_err(|e| KvError::Backend(e.to_string()))
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        let mut conn = self.conn.clone();
        conn.set(key, value)
            .await
            .map_err(|e| KvError::Backend(e.to_string()))
    }

    async fn put_with_ttl(
        &self,
        key: &str,
        value: &[u8],
        ttl: core::time::Duration,
    ) -> Result<(), KvError> {
        let millis = ttl_millis(ttl)?;
        let mut conn = self.conn.clone();
        conn.pset_ex(key, value, millis)
            .await
            .map_err(|e| KvError::Backend(e.to_string()))
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        let mut conn = self.conn.clone();
        conn.del(key)
            .await
            .map_err(|e| KvError::Backend(e.to_string()))
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        let mut conn = self.conn.clone();
        let pattern = scan_pattern(prefix);
        let mut iter: redis::AsyncIter<String> = conn
            .scan_match(pattern)
            .await
            .map_err(|e| KvError::Backend(e.to_string()))?;

        let mut keys = Vec::new();
        while let Some(key) = iter.next_item().await {
            keys.push(key.map_err(|e| KvError::Backend(e.to_string()))?);
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::{scan_pattern, ttl_millis};
    use core::time::Duration;

    #[test]
    fn scan_pattern_without_prefix_matches_everything() {
        assert_eq!(scan_pattern(None), "*");
    }

    #[test]
    fn scan_pattern_appends_wildcard_to_plain_prefix() {
        assert_eq!(scan_pattern(Some("user:")), "user:*");
    }

    #[test]
    fn scan_pattern_escapes_glob_metacharacters() {
        assert_eq!(scan_pattern(Some("a*b")), r"a\*b*");
        assert_eq!(scan_pattern(Some("a?b")), r"a\?b*");
        assert_eq!(scan_pattern(Some("a[1]b")), r"a\[1\]b*");
        assert_eq!(scan_pattern(Some(r"a\b")), r"a\\b*");
        assert_eq!(scan_pattern(Some(r"*?[]\")), r"\*\?\[\]\\*");
    }

    #[test]
    fn ttl_millis_rejects_zero() {
        assert!(ttl_millis(Duration::ZERO).is_err());
    }

    #[test]
    fn ttl_millis_rounds_sub_millisecond_up_to_one() {
        assert_eq!(ttl_millis(Duration::from_micros(50)).unwrap(), 1);
    }

    #[test]
    fn ttl_millis_converts_whole_durations() {
        assert_eq!(ttl_millis(Duration::from_secs(2)).unwrap(), 2000);
    }

    #[test]
    fn ttl_millis_rejects_overflowing_durations() {
        assert!(ttl_millis(Duration::MAX).is_err());
    }
}
