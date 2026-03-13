//! Cloudflare D1 database wrapper.

use serde::de::DeserializeOwned;
use skyzen_services::{DbBackend, DbError, DbExecResult, DbValue};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use worker_sys::{D1Database, D1PreparedStatement, D1Result};

use crate::database_error::{js_err, CfDatabaseError};

/// A Cloudflare D1 database binding.
pub struct CfD1 {
    db: D1Database,
}

impl Clone for CfD1 {
    fn clone(&self) -> Self {
        let js: &JsValue = self.db.as_ref();
        Self {
            db: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfD1 {}
unsafe impl Sync for CfD1 {}

impl std::fmt::Debug for CfD1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfD1").finish_non_exhaustive()
    }
}

impl CfD1 {
    /// Create a `CfD1` from a D1 binding.
    #[must_use]
    pub fn new(binding: JsValue) -> Self {
        Self {
            db: binding.unchecked_into(),
        }
    }

    /// Create a `CfD1` from a Workers env by binding name.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, CfDatabaseError> {
        let binding = crate::ffi::get_binding(env, binding_name).map_err(|error| {
            CfDatabaseError::Backend(format!("failed to get D1 binding '{binding_name}': {error:?}"))
        })?;
        Ok(Self::new(binding))
    }

    /// Run SQL directly via D1 `exec`.
    pub async fn exec(&self, query: &str) -> Result<JsValue, CfDatabaseError> {
        let promise = self.db.exec(query).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)
    }

    /// Prepare a SQL statement.
    pub fn prepare(&self, query: &str) -> Result<CfD1Statement, CfDatabaseError> {
        let stmt = self.db.prepare(query).map_err(js_err)?;
        Ok(CfD1Statement { stmt })
    }
}

/// A prepared D1 SQL statement.
pub struct CfD1Statement {
    stmt: D1PreparedStatement,
}

impl Clone for CfD1Statement {
    fn clone(&self) -> Self {
        let js: &JsValue = self.stmt.as_ref();
        Self {
            stmt: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfD1Statement {}
unsafe impl Sync for CfD1Statement {}

impl std::fmt::Debug for CfD1Statement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfD1Statement").finish_non_exhaustive()
    }
}

impl CfD1Statement {
    /// Bind parameters to this prepared statement.
    pub fn bind(&self, params: &[DbValue]) -> Result<Self, CfDatabaseError> {
        let values = js_sys::Array::new();
        for value in params {
            values.push(&db_value_to_js(value)?);
        }
        let stmt = self.stmt.bind(values).map_err(js_err)?;
        Ok(Self { stmt })
    }

    /// Execute and return all rows (`stmt.all()`).
    pub async fn all(&self) -> Result<JsValue, CfDatabaseError> {
        let promise = self.stmt.all().map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)
    }

    /// Execute and return the first row (`stmt.first()`).
    pub async fn first(&self) -> Result<JsValue, CfDatabaseError> {
        let promise = self.stmt.first(None).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)
    }

    /// Execute and return raw row arrays (`stmt.raw()`).
    pub async fn raw(&self) -> Result<JsValue, CfDatabaseError> {
        let promise = self.stmt.raw().map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)
    }

    /// Execute a write statement (`stmt.run()`).
    pub async fn run(&self) -> Result<JsValue, CfDatabaseError> {
        let promise = self.stmt.run().map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)
    }

    /// Execute `all()` and deserialize the result into a Rust type.
    pub async fn all_json<T: DeserializeOwned>(&self) -> Result<T, CfDatabaseError> {
        let value = self.all().await?;
        serde_wasm_bindgen::from_value(value).map_err(Into::into)
    }

    /// Execute `first()` and deserialize the result into a Rust type.
    pub async fn first_json<T: DeserializeOwned>(&self) -> Result<T, CfDatabaseError> {
        let value = self.first().await?;
        serde_wasm_bindgen::from_value(value).map_err(Into::into)
    }

    /// Execute `raw()` and deserialize the result into a Rust type.
    pub async fn raw_json<T: DeserializeOwned>(&self) -> Result<T, CfDatabaseError> {
        let value = self.raw().await?;
        serde_wasm_bindgen::from_value(value).map_err(Into::into)
    }

    /// Execute `run()` and deserialize the result into a Rust type.
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
            .map_err(|error| DbError::Backend(error.to_string()))?
            .bind(params)
            .map_err(|error| DbError::Backend(error.to_string()))?;
        let value = statement
            .all()
            .await
            .map_err(|error| DbError::Backend(error.to_string()))?;
        d1_result_to_exec_result(value)
    }

    async fn execute(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        let statement = self
            .prepare(query)
            .map_err(|error| DbError::Backend(error.to_string()))?
            .bind(params)
            .map_err(|error| DbError::Backend(error.to_string()))?;
        let value = statement
            .run()
            .await
            .map_err(|error| DbError::Backend(error.to_string()))?;
        d1_result_to_exec_result(value)
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
        .map_err(|error| DbError::Backend(format!("{error:?}")))?;
    if !success {
        let message = result
            .error()
            .map_err(|error| DbError::Backend(format!("{error:?}")))?
            .unwrap_or_else(|| "unknown D1 error".to_owned());
        return Err(DbError::Backend(message));
    }

    let rows = result
        .results()
        .map_err(|error| DbError::Backend(format!("{error:?}")))?
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    serde_wasm_bindgen::from_value(row)
                        .map_err(|error| DbError::Backend(error.to_string()))
                })
                .collect::<Result<Vec<serde_json::Value>, DbError>>()
        })
        .transpose()?
        .unwrap_or_default();

    let meta = result
        .meta()
        .map_err(|error| DbError::Backend(format!("{error:?}")))
        .and_then(|meta| {
            serde_wasm_bindgen::from_value::<D1Meta>(meta.into())
                .map_err(|error| DbError::Backend(error.to_string()))
        })
        .unwrap_or_default();

    Ok(DbExecResult {
        rows_read: meta.rows_read.unwrap_or(rows.len() as u64),
        rows_written: meta.rows_written.or(meta.changes).unwrap_or(0),
        rows,
    })
}

fn db_value_to_js(value: &DbValue) -> Result<JsValue, CfDatabaseError> {
    match value {
        DbValue::Null => Ok(JsValue::NULL),
        DbValue::Boolean(value) => Ok(JsValue::from_bool(*value)),
        DbValue::Integer(value) => integer_to_js(*value),
        DbValue::Real(value) => Ok(JsValue::from_f64(*value)),
        DbValue::Text(value) => Ok(JsValue::from_str(value)),
        DbValue::Blob(value) => Ok(js_sys::Uint8Array::from(value.as_slice()).into()),
    }
}

fn integer_to_js(value: i64) -> Result<JsValue, CfDatabaseError> {
    const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
    const MIN_SAFE_INTEGER: i64 = -9_007_199_254_740_991;

    if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
        return Err(CfDatabaseError::Backend(format!(
            "D1 integer parameter exceeds JavaScript safe integer range: {value}"
        )));
    }

    #[allow(clippy::cast_precision_loss)]
    {
        Ok(JsValue::from_f64(value as f64))
    }
}
