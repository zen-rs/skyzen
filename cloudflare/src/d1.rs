//! Cloudflare D1 database wrapper.
//!
//! # Transactions
//!
//! D1 runs every statement in auto-commit and offers no interactive transaction: there is no
//! connection for a Worker to hold open across `BEGIN` … `COMMIT`. [`CfD1::batch`] — surfaced
//! portably as [`Db::execute_batch`](skyzen_services::Db::execute_batch) — is D1's transaction
//! primitive, and the whole sequence rolls back if any statement in it fails.
//! [`DbBackend::begin`] therefore keeps its
//! [`TransactionsUnsupported`](DbError::TransactionsUnsupported) default rather than faking one
//! with `exec("BEGIN")`, which Cloudflare documents as unsafe outside maintenance.
//!
//! # Sessions API
//!
//! D1's read replication (`withSession`) is not wrapped. Using it correctly means threading a
//! *bookmark* — the token naming how far a replica must have caught up — out of every response and
//! back into the next request, which is a cross-request contract rather than a method call: it has
//! to be modelled in the framework's request/response types before a wrapper here would be more
//! than a footgun that silently serves stale reads.

use serde::de::DeserializeOwned;
use skyzen_services::{BatchStatement, DbBackend, DbError, DbExecResult, DbValue};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use worker::send::IntoSendFuture;
use worker_sys::{D1Database, D1PreparedStatement, D1Result};

use crate::database_error::{integer_to_js_number, js_err, CfDatabaseError};

/// A Cloudflare D1 database binding.
pub struct CfD1 {
    db: D1Database,
}

impl_js_handle_traits!(CfD1 { db });

impl CfD1 {
    /// Create a `CfD1` from a D1 binding.
    ///
    /// The binding is not validated here; an invalid binding surfaces as a
    /// JS error on first use. Prefer [`CfD1::from_env`], which checks that
    /// the binding looks like a D1 database.
    #[must_use]
    pub fn new(binding: JsValue) -> Self {
        Self {
            db: binding.unchecked_into(),
        }
    }

    /// Create a `CfD1` from a Workers env by binding name.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError::Backend`] if the binding cannot be found
    /// or does not look like a D1 database.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, CfDatabaseError> {
        let binding = crate::ffi::get_binding(env, binding_name).map_err(|error| {
            CfDatabaseError::Backend(format!(
                "failed to get D1 binding '{binding_name}': {error:?}"
            ))
        })?;
        crate::ffi::require_methods(&binding, binding_name, &["prepare", "exec"])
            .map_err(js_err)?;
        Ok(Self::new(binding))
    }

    /// Run SQL directly via D1 `exec`.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution fails.
    pub async fn exec(&self, query: &str) -> Result<JsValue, CfDatabaseError> {
        let promise = self.db.exec(query).map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)
    }

    /// Prepare a SQL statement.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when the statement cannot be prepared.
    pub fn prepare(&self, query: &str) -> Result<CfD1Statement, CfDatabaseError> {
        let stmt = self.db.prepare(query).map_err(js_err)?;
        Ok(CfD1Statement { stmt })
    }

    /// Run a sequence of prepared statements as one transaction.
    ///
    /// This is D1's only atomicity primitive: the statements run in order, and if one fails the
    /// whole sequence is rolled back. The returned values are the raw `D1Result` objects, one per
    /// statement, in the same order — the same shape [`CfD1Statement::all`] hands back.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when the runtime rejects the batch or a statement fails.
    pub async fn batch(
        &self,
        statements: &[CfD1Statement],
    ) -> Result<Vec<JsValue>, CfDatabaseError> {
        let array = js_sys::Array::new();
        for statement in statements {
            array.push(statement.stmt.as_ref());
        }

        let promise = self.db.batch(array).map_err(js_err)?;
        let results = JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(js_sys::Array::from(&results).iter().collect())
    }
}

/// A prepared D1 SQL statement.
pub struct CfD1Statement {
    stmt: D1PreparedStatement,
}

impl_js_handle_traits!(CfD1Statement { stmt });

impl CfD1Statement {
    /// Bind parameters to this prepared statement.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when a parameter cannot be converted or
    /// binding fails.
    pub fn bind(&self, params: &[DbValue]) -> Result<Self, CfDatabaseError> {
        let values = js_sys::Array::new();
        for value in params {
            values.push(&db_value_to_js(value)?);
        }
        let stmt = self.stmt.bind(values).map_err(js_err)?;
        Ok(Self { stmt })
    }

    /// Execute and return all rows (`stmt.all()`).
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution fails.
    pub async fn all(&self) -> Result<JsValue, CfDatabaseError> {
        let promise = self.stmt.all().map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)
    }

    /// Execute and return the first row (`stmt.first()`).
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution fails.
    pub async fn first(&self) -> Result<JsValue, CfDatabaseError> {
        let promise = self.stmt.first(None).map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)
    }

    /// Execute and return raw row arrays (`stmt.raw()`).
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution fails.
    pub async fn raw(&self) -> Result<JsValue, CfDatabaseError> {
        let promise = self.stmt.raw().map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)
    }

    /// Execute a write statement (`stmt.run()`).
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution fails.
    pub async fn run(&self) -> Result<JsValue, CfDatabaseError> {
        let promise = self.stmt.run().map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)
    }

    /// Execute `all()` and deserialize the result rows (`.results`) into a
    /// vector of Rust values.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution or row deserialization
    /// fails.
    pub async fn all_json<T: DeserializeOwned>(&self) -> Result<Vec<T>, CfDatabaseError> {
        let value = self.all().await?;
        let result: D1Result = value.unchecked_into();
        let rows = result.results().map_err(js_err)?.unwrap_or_default();
        rows.iter()
            .map(|row| serde_wasm_bindgen::from_value(row).map_err(Into::into))
            .collect()
    }

    /// Execute `first()` and deserialize the row into a Rust type.
    ///
    /// Returns `Ok(None)` when the query matches no rows.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution or deserialization fails.
    pub async fn first_json<T: DeserializeOwned>(&self) -> Result<Option<T>, CfDatabaseError> {
        let value = self.first().await?;
        if value.is_null() || value.is_undefined() {
            return Ok(None);
        }
        serde_wasm_bindgen::from_value(value)
            .map(Some)
            .map_err(Into::into)
    }

    /// Execute `raw()` and deserialize the result into a Rust type.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution or deserialization fails.
    pub async fn raw_json<T: DeserializeOwned>(&self) -> Result<T, CfDatabaseError> {
        let value = self.raw().await?;
        serde_wasm_bindgen::from_value(value).map_err(Into::into)
    }

    /// Execute `run()` and deserialize the result into a Rust type.
    ///
    /// # Errors
    ///
    /// Returns [`CfDatabaseError`] when execution or deserialization fails.
    pub async fn run_json<T: DeserializeOwned>(&self) -> Result<T, CfDatabaseError> {
        let value = self.run().await?;
        serde_wasm_bindgen::from_value(value).map_err(Into::into)
    }
}

impl DbBackend for CfD1 {
    fn dialect(&self) -> skyzen_services::sql::DbDialect {
        skyzen_services::sql::DbDialect::Sqlite
    }

    async fn query(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        let statement = self
            .prepare(query)
            .map_err(|error| DbError::backend(error.to_string()))?
            .bind(params)
            .map_err(|error| DbError::backend(error.to_string()))?;
        let value = statement
            .all()
            .await
            .map_err(|error| DbError::backend(error.to_string()))?;
        d1_result_to_exec_result(value)
    }

    async fn execute(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        let statement = self
            .prepare(query)
            .map_err(|error| DbError::backend(error.to_string()))?
            .bind(params)
            .map_err(|error| DbError::backend(error.to_string()))?;
        let value = statement
            .run()
            .await
            .map_err(|error| DbError::backend(error.to_string()))?;
        d1_result_to_exec_result(value)
    }

    /// Run the batch through D1's `batch()`, which is a transaction.
    ///
    /// Nothing here has to unwind on failure the way the native sqlx backend does: D1 rolls the
    /// whole sequence back itself and reports the statement that failed.
    async fn execute_batch(
        &self,
        statements: Vec<BatchStatement>,
    ) -> Result<Vec<DbExecResult>, DbError> {
        let prepared = statements
            .iter()
            .map(|statement| {
                self.prepare(&statement.sql)
                    .and_then(|prepared| prepared.bind(&statement.params))
                    .map_err(|error| DbError::backend(error.to_string()))
            })
            .collect::<Result<Vec<_>, DbError>>()?;

        let results = self
            .batch(&prepared)
            .await
            .map_err(|error| DbError::backend(error.to_string()))?;

        if results.len() != prepared.len() {
            return Err(DbError::backend(format!(
                "D1 batch returned {} results for {} statements",
                results.len(),
                prepared.len()
            )));
        }

        results.into_iter().map(d1_result_to_exec_result).collect()
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct D1Meta {
    rows_read: Option<u64>,
    rows_written: Option<u64>,
    changes: Option<u64>,
}

fn d1_result_to_exec_result(value: JsValue) -> Result<DbExecResult, DbError> {
    let result: D1Result = value.unchecked_into();
    let success = result
        .success()
        .map_err(|error| DbError::backend(format!("{error:?}")))?;
    if !success {
        let message = result
            .error()
            .map_err(|error| DbError::backend(format!("{error:?}")))?
            .unwrap_or_else(|| "unknown D1 error".to_owned());
        return Err(DbError::backend(message));
    }

    let rows = result
        .results()
        .map_err(|error| DbError::backend(format!("{error:?}")))?
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    serde_wasm_bindgen::from_value(row)
                        .map_err(|error| DbError::backend(error.to_string()))
                })
                .collect::<Result<Vec<serde_json::Value>, DbError>>()
        })
        .transpose()?
        .unwrap_or_default();

    let meta = result
        .meta()
        .map_err(|error| DbError::backend(format!("{error:?}")))
        .and_then(|meta| {
            serde_wasm_bindgen::from_value::<D1Meta>(meta.into())
                .map_err(|error| DbError::backend(error.to_string()))
        })
        .unwrap_or_default();

    Ok(DbExecResult {
        rows_read: meta.rows_read.unwrap_or(rows.len() as u64),
        rows_written: meta.rows_written.or(meta.changes).unwrap_or(0),
        rows,
    })
}

/// Convert a bound parameter to the JS value D1 accepts.
///
/// D1's binding API takes only `null`, numbers, strings and `ArrayBuffer`s, so the richer
/// [`DbValue`] variants are rendered in the textual form `SQLite` compares and stores them as:
/// RFC 3339 for a timestamp, the hyphenated form for a UUID, the exact decimal rendering for a
/// decimal, and compact JSON for a document — the last of which `SQLite`'s JSON functions read
/// directly.
fn db_value_to_js(value: &DbValue) -> Result<JsValue, CfDatabaseError> {
    match value {
        DbValue::Null => Ok(JsValue::NULL),
        DbValue::Boolean(value) => Ok(JsValue::from_bool(*value)),
        DbValue::Integer(value) => integer_to_js(*value),
        DbValue::Real(value) => Ok(JsValue::from_f64(*value)),
        DbValue::Text(value) => Ok(JsValue::from_str(value)),
        DbValue::Blob(value) => Ok(js_sys::Uint8Array::from(value.as_slice()).into()),
        DbValue::Timestamp(value) => Ok(JsValue::from_str(&value.to_rfc3339())),
        DbValue::Uuid(value) => Ok(JsValue::from_str(&value.to_string())),
        DbValue::Decimal(value) => Ok(JsValue::from_str(&value.to_string())),
        DbValue::Json(value) => Ok(JsValue::from_str(&value.to_string())),
    }
}

fn integer_to_js(value: i64) -> Result<JsValue, CfDatabaseError> {
    integer_to_js_number(value).map_err(|message| CfDatabaseError::Backend(format!("D1 {message}")))
}
