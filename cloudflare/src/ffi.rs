//! Low-level FFI bindings for Cloudflare Workers APIs.
//!
//! Most types are provided by `worker-sys`. This module only contains
//! bindings that `worker-sys` does not expose (KV Namespace) and the
//! `get_binding` helper.

use js_sys::Promise;
use wasm_bindgen::prelude::*;

// ── KV Namespace ──

/// Cloudflare KV Namespace binding.
#[wasm_bindgen]
extern "C" {
    /// KV namespace type from the Workers runtime.
    pub type KvNamespace;

    /// Get a value by key. Returns a promise that resolves to the value or null.
    #[wasm_bindgen(method, catch)]
    pub fn get(this: &KvNamespace, key: &str, options: &JsValue) -> Result<Promise, JsValue>;

    /// Put a key-value pair. Returns a promise.
    #[wasm_bindgen(method, catch)]
    pub fn put(this: &KvNamespace, key: &str, value: &JsValue) -> Result<Promise, JsValue>;

    /// Delete a key. Returns a promise.
    #[wasm_bindgen(method, catch)]
    pub fn delete(this: &KvNamespace, key: &str) -> Result<Promise, JsValue>;

    /// List keys. Returns a promise that resolves to a list result.
    #[wasm_bindgen(method, catch)]
    pub fn list(this: &KvNamespace, options: &JsValue) -> Result<Promise, JsValue>;
}

// ── Helpers ──

/// Get a binding from the Workers env object by name.
///
/// # Errors
///
/// Returns `JsValue` error if the binding cannot be accessed or does not exist.
pub fn get_binding(env: &JsValue, name: &str) -> Result<JsValue, JsValue> {
    let binding = js_sys::Reflect::get(env, &JsValue::from_str(name))?;
    if binding.is_undefined() || binding.is_null() {
        return Err(JsValue::from_str(&format!(
            "missing Cloudflare Workers binding '{name}'"
        )));
    }
    Ok(binding)
}
