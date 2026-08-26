//! Cloudflare Durable Object `SQLite` adapter for [`DurableDbBackend`].

use core::future::{ready, Future};
use serde_json::Value;
use skyzen_services::{
    durable::sql::{DurableDbBackend, DurableDbError},
    DbExecResult, DbValue,
};
use wasm_bindgen::JsValue;
use worker_sys::{DurableObjectState, SqlStorage};

use crate::database_error::integer_to_js_number;

/// Cloudflare Durable Object SQL store backed by `state.storage.sql`.
pub struct CfDurableDb {
    sql: SqlStorage,
}

impl_js_handle_traits!(CfDurableDb { sql });

impl CfDurableDb {
    /// Create from a raw SQL storage handle.
    #[must_use]
    pub const fn new(sql: SqlStorage) -> Self {
        Self { sql }
    }

    /// Create from Durable Object state.
    ///
    /// # Errors
    ///
    /// Returns [`DurableDbError`] if `state.storage` cannot be read.
    pub fn from_state(state: &DurableObjectState) -> Result<Self, DurableDbError> {
        let storage = state.storage().map_err(js_err)?;
        Ok(Self::new(storage.sql()))
    }
}

impl CfDurableDb {
    /// Run one statement against the Durable Object's `SQLite` storage.
    ///
    /// The storage API is synchronous, so this does the whole job; the trait methods only wrap the
    /// result in a ready future.
    fn exec(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DurableDbError> {
        let bindings = js_sys::Array::new();
        for value in params {
            bindings.push(&db_value_to_js(value)?);
        }

        let cursor = self.sql.exec(query, bindings).map_err(js_err)?;
        let rows_array = cursor.to_array();

        let mut rows = Vec::with_capacity(rows_array.length() as usize);
        for index in 0..rows_array.length() {
            let row = rows_array.get(index);
            let row: Value = serde_wasm_bindgen::from_value(row).map_err(|error| {
                DurableDbError::backend(format!("failed to deserialize sql row: {error}"))
            })?;
            rows.push(row);
        }

        Ok(DbExecResult {
            rows,
            rows_read: f64_to_u64(cursor.rows_read(), "rowsRead")?,
            rows_written: f64_to_u64(cursor.rows_written(), "rowsWritten")?,
        })
    }
}

// The Durable Object storage API is synchronous, so each future is ready on creation rather than
// an `async` block with nothing to await.
impl DurableDbBackend for CfDurableDb {
    fn query(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + Send {
        ready(self.exec(query, params))
    }

    fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + Send {
        ready(self.exec(query, params))
    }

    fn database_size(&self) -> impl Future<Output = Result<u64, DurableDbError>> + Send {
        ready(f64_to_u64(self.sql.database_size(), "databaseSize"))
    }
}

/// Convert a bound parameter to the JS value Durable Object SQL accepts.
///
/// The storage API takes only `null`, numbers, strings and `ArrayBuffer`s, so the richer
/// [`DbValue`] variants are rendered in the textual form `SQLite` compares and stores them as:
/// RFC 3339 for a timestamp, the hyphenated form for a UUID, the exact decimal rendering for a
/// decimal, and compact JSON for a document.
fn db_value_to_js(value: &DbValue) -> Result<JsValue, DurableDbError> {
    match value {
        DbValue::Null => Ok(JsValue::NULL),
        DbValue::Boolean(v) => Ok(JsValue::from_bool(*v)),
        DbValue::Integer(v) => integer_to_js_number(*v)
            .map_err(|message| DurableDbError::backend(format!("durable sql {message}"))),
        DbValue::Real(v) => Ok(JsValue::from_f64(*v)),
        DbValue::Text(v) => Ok(JsValue::from_str(v)),
        DbValue::Blob(v) => Ok(js_sys::Uint8Array::from(v.as_slice()).into()),
        DbValue::Timestamp(v) => Ok(JsValue::from_str(&v.to_rfc3339())),
        DbValue::Uuid(v) => Ok(JsValue::from_str(&v.to_string())),
        DbValue::Decimal(v) => Ok(JsValue::from_str(&v.to_string())),
        DbValue::Json(v) => Ok(JsValue::from_str(&v.to_string())),
    }
}

fn f64_to_u64(value: f64, source: &str) -> Result<u64, DurableDbError> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return Err(DurableDbError::backend(format!(
            "{source} returned invalid numeric value: {value}"
        )));
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        Ok(value as u64)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn js_err(error: JsValue) -> DurableDbError {
    DurableDbError::backend(format!("{error:?}"))
}
