//! Cloudflare Durable Object `SQLite` adapter for [`DurableSqlStore`].

use serde_json::Value;
use skyzen_services::durable::sql::{DurableSqlError, DurableSqlStore, SqlResult, SqlValue};
use wasm_bindgen::{JsCast, JsValue};
use worker_sys::{DurableObjectState, SqlStorage};

/// Cloudflare Durable Object SQL store backed by `state.storage.sql`.
pub struct CfDurableSql {
    sql: SqlStorage,
}

impl Clone for CfDurableSql {
    fn clone(&self) -> Self {
        let js: &JsValue = self.sql.as_ref();
        Self {
            sql: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfDurableSql {}
unsafe impl Sync for CfDurableSql {}

impl std::fmt::Debug for CfDurableSql {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfDurableSql").finish_non_exhaustive()
    }
}

impl CfDurableSql {
    /// Create from a raw SQL storage handle.
    #[must_use]
    pub const fn new(sql: SqlStorage) -> Self {
        Self { sql }
    }

    /// Create from Durable Object state.
    ///
    /// # Errors
    ///
    /// Returns [`DurableSqlError`] if `state.storage` cannot be read.
    pub fn from_state(state: &DurableObjectState) -> Result<Self, DurableSqlError> {
        let storage = state.storage().map_err(js_err)?;
        Ok(Self::new(storage.sql()))
    }
}

impl DurableSqlStore for CfDurableSql {
    async fn exec(&self, query: &str, params: &[SqlValue]) -> Result<SqlResult, DurableSqlError> {
        let bindings = js_sys::Array::new();
        for value in params {
            bindings.push(&sql_value_to_js(value));
        }

        let cursor = self.sql.exec(query, bindings).map_err(js_err)?;
        let rows_array = cursor.to_array();

        let mut rows = Vec::with_capacity(rows_array.length() as usize);
        for index in 0..rows_array.length() {
            let row = rows_array.get(index);
            let row: Value = serde_wasm_bindgen::from_value(row).map_err(|error| {
                DurableSqlError::Backend(format!("failed to deserialize sql row: {error}"))
            })?;
            rows.push(row);
        }

        Ok(SqlResult {
            rows,
            rows_read: f64_to_u64(cursor.rows_read(), "rowsRead")?,
            rows_written: f64_to_u64(cursor.rows_written(), "rowsWritten")?,
        })
    }

    async fn database_size(&self) -> Result<u64, DurableSqlError> {
        f64_to_u64(self.sql.database_size(), "databaseSize")
    }
}

fn sql_value_to_js(value: &SqlValue) -> JsValue {
    match value {
        SqlValue::Null => JsValue::NULL,
        SqlValue::Boolean(v) => JsValue::from_bool(*v),
        #[allow(clippy::cast_precision_loss)]
        SqlValue::Integer(v) => JsValue::from_f64(*v as f64),
        SqlValue::Real(v) => JsValue::from_f64(*v),
        SqlValue::Text(v) => JsValue::from_str(v),
        SqlValue::Blob(v) => js_sys::Uint8Array::from(v.as_slice()).into(),
    }
}

fn f64_to_u64(value: f64, source: &str) -> Result<u64, DurableSqlError> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return Err(DurableSqlError::Backend(format!(
            "{source} returned invalid numeric value: {value}"
        )));
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        Ok(value as u64)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn js_err(error: JsValue) -> DurableSqlError {
    DurableSqlError::Backend(format!("{error:?}"))
}
