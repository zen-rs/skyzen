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

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        let mut conn = self.conn.clone();
        conn.del(key)
            .await
            .map_err(|e| KvError::Backend(e.to_string()))
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        let mut conn = self.conn.clone();
        let pattern = prefix.map_or_else(|| "*".to_owned(), |p| format!("{p}*"));
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
