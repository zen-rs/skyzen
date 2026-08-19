//! Shared error type for Cloudflare database integrations.

use wasm_bindgen::prelude::*;

/// Errors returned by Cloudflare database wrappers (`D1`, Durable `SQLite`).
#[derive(Debug, thiserror::Error)]
pub enum CfDatabaseError {
    /// The underlying Cloudflare runtime/JS API returned an error.
    #[error("cloudflare database error: {0}")]
    Backend(String),

    /// Failed to deserialize JS values into Rust types.
    #[error("cloudflare database serialization error: {0}")]
    Serialization(#[from] serde_wasm_bindgen::Error),
}

/// Convert a `JsValue` error into [`CfDatabaseError`].
///
/// Takes ownership to match `Result<_, JsValue>::map_err` signature.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn js_err(e: JsValue) -> CfDatabaseError {
    CfDatabaseError::Backend(format!("{e:?}"))
}

/// Convert an `i64` SQL parameter into a JS number, rejecting values outside
/// the JS safe-integer range (`±2^53 - 1`) that would silently lose
/// precision. Shared by the D1 and Durable Object `SQLite` backends.
pub(crate) fn integer_to_js_number(value: i64) -> Result<JsValue, String> {
    const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
    const MIN_SAFE_INTEGER: i64 = -9_007_199_254_740_991;

    if !(MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
        return Err(format!(
            "integer parameter exceeds JavaScript safe integer range: {value}"
        ));
    }

    #[allow(clippy::cast_precision_loss)]
    {
        Ok(JsValue::from_f64(value as f64))
    }
}
