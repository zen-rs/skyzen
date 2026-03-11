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
