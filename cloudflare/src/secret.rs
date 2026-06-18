//! Typed readers for Cloudflare Workers string `vars` / secrets bindings.
//!
//! Cloudflare Workers expose `vars` and secrets to wasm code as JS-side
//! string properties on the `env` object. Reading them ergonomically requires
//! `js_sys::Reflect::get` + `JsValue::as_string`, which is awkward and
//! repetitive. The functions below wrap that dance so consumers never have
//! to handle `JsValue` directly.

use js_sys::Reflect;
use wasm_bindgen::JsValue;

/// Errors raised when reading a Cloudflare Workers string binding.
#[derive(Debug, thiserror::Error)]
pub enum CfSecretError {
    /// `Reflect::get` failed (typically: `env` is not the expected object).
    #[error("read Cloudflare Workers binding `{binding}`")]
    Reflect {
        /// Binding name being read.
        binding: String,
    },
    /// The binding is missing (undefined or null).
    #[error("Cloudflare Workers binding `{binding}` is missing")]
    Missing {
        /// Binding name being read.
        binding: String,
    },
    /// The binding exists but isn't a string value.
    #[error("Cloudflare Workers binding `{binding}` must be a string")]
    NotString {
        /// Binding name being read.
        binding: String,
    },
}

/// Read a required string binding. Returns an error when the binding is
/// missing or non-string.
pub fn required_string(env: &JsValue, binding: &str) -> Result<String, CfSecretError> {
    let value =
        Reflect::get(env, &JsValue::from_str(binding)).map_err(|_| CfSecretError::Reflect {
            binding: binding.to_owned(),
        })?;
    if value.is_undefined() || value.is_null() {
        return Err(CfSecretError::Missing {
            binding: binding.to_owned(),
        });
    }
    value.as_string().ok_or_else(|| CfSecretError::NotString {
        binding: binding.to_owned(),
    })
}

/// Read an optional string binding. Returns `Ok(None)` when the binding is
/// undefined or null. Returns `Err` only when the binding is present but not
/// a string.
pub fn optional_string(env: &JsValue, binding: &str) -> Result<Option<String>, CfSecretError> {
    let Ok(value) = Reflect::get(env, &JsValue::from_str(binding)) else {
        return Ok(None);
    };
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value
        .as_string()
        .map(Some)
        .ok_or_else(|| CfSecretError::NotString {
            binding: binding.to_owned(),
        })
}
