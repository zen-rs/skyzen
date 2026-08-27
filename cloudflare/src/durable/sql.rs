//! Cloudflare Durable Object `SQLite` adapter for [`DurableDbBackend`].

use core::future::{ready, Future};
use serde_json::Value;
use skyzen_services::{
    durable::sql::{DurableDbBackend, DurableDbError},
    DbExecResult, DbValue,
};
use wasm_bindgen::JsValue;
use worker_sys::{DurableObjectState, SqlStorage, SqlStorageCursor};

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

    /// Run one statement and walk its rows one at a time, instead of materializing them all.
    ///
    /// [`DurableDbBackend::query`] collects every row into a `Vec` before returning, which is the
    /// portable shape but the wrong one for a scan over a large table: the whole result set sits
    /// in the isolate's memory at once. The platform's cursor is a real JavaScript iterator, so
    /// this walks it with `next()` and yields each row as it arrives.
    ///
    /// The cursor also carries what the portable result cannot: the statement's
    /// [`column_names`](CfSqlCursor::column_names), which is the only way to see the columns of a
    /// row that came back empty.
    ///
    /// # Errors
    ///
    /// Returns [`DurableDbError`] if a bound parameter cannot be represented or the statement
    /// itself fails. Failures while decoding a row surface on the iterator, not here.
    pub fn exec_cursor(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> Result<CfSqlCursor, DurableDbError> {
        let bindings = js_sys::Array::new();
        for value in params {
            bindings.push(&db_value_to_js(value)?);
        }

        let cursor = self.sql.exec(query, bindings).map_err(js_err)?;
        Ok(CfSqlCursor { cursor })
    }
}

/// A cursor over the rows of one Durable Object SQL statement.
///
/// Yields each row as a JSON object, in the order the statement produced them, without holding the
/// whole result set. The cursor is consumed as it is walked: a row it has yielded is gone.
pub struct CfSqlCursor {
    cursor: SqlStorageCursor,
}

// SAFETY: Workers WASM executes on a single thread, so the JS handle inside is safe to mark
// `Send`/`Sync`. Written out rather than taken from `impl_js_handle_traits!` because that macro
// also derives `Clone`, and a clone of a cursor would silently share one iteration state with the
// original — the opposite of what cloning an iterator reads as.
unsafe impl Send for CfSqlCursor {}
// SAFETY: see above.
unsafe impl Sync for CfSqlCursor {}

impl std::fmt::Debug for CfSqlCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfSqlCursor").finish_non_exhaustive()
    }
}

impl CfSqlCursor {
    /// The statement's column names, in the order the platform reports them.
    ///
    /// Available before the first row and on a result set with no rows at all, which is what makes
    /// it worth exposing: an empty `Vec` of rows says nothing about the shape of what was queried.
    #[must_use]
    pub fn column_names(&self) -> Vec<String> {
        self.cursor
            .column_names()
            .iter()
            .filter_map(|name| name.as_string())
            .collect()
    }

    /// How many rows this statement has read *so far*.
    ///
    /// The counter tracks the cursor's progress, so on a half-walked cursor it reports a partial
    /// number. Read it after the iteration finishes to bill the whole statement.
    ///
    /// # Errors
    ///
    /// Returns [`DurableDbError`] if the runtime reports a value that is not a whole count.
    pub fn rows_read(&self) -> Result<u64, DurableDbError> {
        f64_to_u64(self.cursor.rows_read(), "rowsRead")
    }

    /// How many rows this statement has written so far. See [`rows_read`](Self::rows_read) for
    /// when to read it.
    ///
    /// # Errors
    ///
    /// Returns [`DurableDbError`] if the runtime reports a value that is not a whole count.
    pub fn rows_written(&self) -> Result<u64, DurableDbError> {
        f64_to_u64(self.cursor.rows_written(), "rowsWritten")
    }
}

impl Iterator for CfSqlCursor {
    type Item = Result<Value, DurableDbError>;

    fn next(&mut self) -> Option<Self::Item> {
        let step = self.cursor.next();

        let done = match js_sys::Reflect::get(&step, &JsValue::from_str("done")) {
            Ok(done) => done.as_bool(),
            Err(error) => return Some(Err(js_err(error))),
        };

        match done {
            Some(true) => None,
            Some(false) => Some(decode_row(&step)),
            // The iterator protocol guarantees the flag; its absence means the runtime handed back
            // something that is not a cursor step, which is worth reporting rather than reading as
            // the end of the rows.
            None => Some(Err(DurableDbError::backend(
                "SqlStorageCursor.next() returned an object without a `done` flag",
            ))),
        }
    }
}

/// Pull the `value` out of one `{ done, value }` cursor step.
fn decode_row(step: &js_sys::Object) -> Result<Value, DurableDbError> {
    let row = js_sys::Reflect::get(step, &JsValue::from_str("value")).map_err(js_err)?;
    serde_wasm_bindgen::from_value(row)
        .map_err(|error| DurableDbError::backend(format!("failed to deserialize sql row: {error}")))
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
