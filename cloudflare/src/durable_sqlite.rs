//! Durable Object `SQLite` wrapper for Cloudflare Workers.

use serde::de::DeserializeOwned;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use worker_sys::SqlStorage;

use crate::database_error::{js_err, CfDatabaseError};

/// A Durable Object `SQLite` handle (`state.storage.sql`).
///
/// This wrapper is designed for code running inside a Durable Object, where
/// the `state` object is available.
pub struct CfDurableSqlite {
    sql: SqlStorage,
}

impl Clone for CfDurableSqlite {
    fn clone(&self) -> Self {
        let js: &JsValue = self.sql.as_ref();
        Self {
            sql: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfDurableSqlite {}
unsafe impl Sync for CfDurableSqlite {}

impl std::fmt::Debug for CfDurableSqlite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfDurableSqlite").finish_non_exhaustive()
    }
}

impl CfDurableSqlite {
    /// Create from a raw `SqlStorage` binding (`state.storage.sql`).
    ///
    /// # Panics
    ///
    /// Panics if the binding is not a valid `SqlStorage` object.
    #[must_use]
    pub fn new(binding: JsValue) -> Self {
        Self {
            sql: binding.unchecked_into(),
        }
    }

    /// Build from Durable Object state (`state.storage.sql`).
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError::Backend`] when `storage` or `storage.sql`
    /// cannot be read.
    pub fn from_state(state: &worker_sys::DurableObjectState) -> Result<Self, CfDatabaseError> {
        let storage = state.storage().map_err(js_err)?;
        Ok(Self { sql: storage.sql() })
    }

    /// Execute SQL against Durable Object `SQLite`.
    ///
    /// Returns the cursor as a `JsValue` for flexible consumption.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution fails.
    pub fn exec(&self, query: &str) -> Result<JsValue, CfDatabaseError> {
        let cursor = self.sql.exec(query, js_sys::Array::new()).map_err(js_err)?;
        Ok(cursor.into())
    }

    /// Execute SQL and deserialize the result into a Rust type.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution or deserialization fails.
    pub fn exec_json<T: DeserializeOwned>(&self, query: &str) -> Result<T, CfDatabaseError> {
        let value = self.exec(query)?;
        serde_wasm_bindgen::from_value(value).map_err(Into::into)
    }
}
