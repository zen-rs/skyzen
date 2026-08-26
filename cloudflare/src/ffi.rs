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

    /// Put a key-value pair with options (e.g. `{ expirationTtl }`). Pass
    /// `JsValue::UNDEFINED` as `options` for a plain put. Returns a promise.
    #[wasm_bindgen(method, catch)]
    pub fn put(
        this: &KvNamespace,
        key: &str,
        value: &JsValue,
        options: &JsValue,
    ) -> Result<Promise, JsValue>;

    /// Get a value together with the metadata stored alongside it. Returns a promise resolving to
    /// `{ value, metadata }`, where `value` is null when the key is absent.
    #[wasm_bindgen(method, catch, js_name = getWithMetadata)]
    pub fn get_with_metadata(
        this: &KvNamespace,
        key: &str,
        options: &JsValue,
    ) -> Result<Promise, JsValue>;

    /// Delete a key. Returns a promise.
    #[wasm_bindgen(method, catch)]
    pub fn delete(this: &KvNamespace, key: &str) -> Result<Promise, JsValue>;

    /// List keys. Returns a promise that resolves to a list result.
    #[wasm_bindgen(method, catch)]
    pub fn list(this: &KvNamespace, options: &JsValue) -> Result<Promise, JsValue>;
}

// ── Durable Objects ──

/// Durable Object methods `worker-sys` does not bind.
///
/// The typed `worker_sys::DurableObjectState` and `DurableObjectNamespace` are cast into these
/// with `unchecked_ref` at the call site; they describe the same JS objects.
#[wasm_bindgen]
extern "C" {
    /// `DurableObjectState`, seen through the methods missing from `worker-sys`.
    #[wasm_bindgen(extends = js_sys::Object)]
    pub type DurableObjectStateExt;

    /// Run `callback` with the object's input gates closed, so no other event interleaves with it.
    /// Returns a promise that resolves when the callback's own promise does.
    #[wasm_bindgen(method, catch, js_name = blockConcurrencyWhile)]
    pub fn block_concurrency_while(
        this: &DurableObjectStateExt,
        callback: &js_sys::Function,
    ) -> Result<Promise, JsValue>;

    /// `DurableObjectNamespace`, seen through the methods missing from `worker-sys`.
    #[wasm_bindgen(extends = js_sys::Object)]
    pub type DurableObjectNamespaceExt;

    /// Restrict the namespace to a data-residency jurisdiction, returning a new namespace.
    #[wasm_bindgen(method, catch)]
    pub fn jurisdiction(
        this: &DurableObjectNamespaceExt,
        jurisdiction: &str,
    ) -> Result<JsValue, JsValue>;
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

/// Duck-type check that a binding exposes the given methods.
///
/// A wrongly typed binding (e.g. an R2 bucket wired to a KV service) fails
/// with a clear error at startup instead of an opaque JS error on first use.
///
/// # Errors
///
/// Returns `JsValue` naming the first missing method.
pub fn require_methods(binding: &JsValue, name: &str, methods: &[&str]) -> Result<(), JsValue> {
    for method in methods {
        let value = js_sys::Reflect::get(binding, &JsValue::from_str(method))?;
        if !value.is_function() {
            return Err(JsValue::from_str(&format!(
                "Cloudflare Workers binding '{name}' has no `{method}` method; \
                 is it bound to the right service type?"
            )));
        }
    }
    Ok(())
}
