//! Cloudflare KV implementation of [`KeyValueStore`].

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use skyzen_services::kv::{KeyValueStore, KvError};

use crate::ffi;

/// A Cloudflare Workers KV namespace.
///
/// Wraps the KV namespace binding from the Workers environment.
///
/// # Safety
///
/// WASM in Workers is single-threaded, so `Send` and `Sync` are safe.
pub struct CfKv {
    ns: ffi::KvNamespace,
}

impl Clone for CfKv {
    fn clone(&self) -> Self {
        let js: &JsValue = self.ns.as_ref();
        Self {
            ns: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfKv {}
unsafe impl Sync for CfKv {}

impl std::fmt::Debug for CfKv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfKv").finish_non_exhaustive()
    }
}

impl CfKv {
    /// Create a `CfKv` from a KV namespace binding.
    ///
    /// # Panics
    ///
    /// Panics if the binding is not a valid KV namespace.
    #[must_use]
    pub fn new(binding: JsValue) -> Self {
        Self {
            ns: binding.unchecked_into(),
        }
    }

    /// Create a `CfKv` from a Workers env by binding name.
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Backend`] if the binding cannot be found.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, KvError> {
        let binding = ffi::get_binding(env, binding_name).map_err(|e| {
            KvError::Backend(format!("failed to get KV binding '{binding_name}': {e:?}"))
        })?;
        Ok(Self::new(binding))
    }
}

impl KeyValueStore for CfKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let options = js_sys::Object::new();
        js_sys::Reflect::set(&options, &"type".into(), &"arrayBuffer".into())
            .map_err(|e| KvError::Backend(format!("{e:?}")))?;

        let promise = self.ns.get(key, &options).map_err(js_err)?;
        let result = JsFuture::from(promise).await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }

        let array = js_sys::Uint8Array::new(&result);
        Ok(Some(array.to_vec()))
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        let array = js_sys::Uint8Array::from(value);
        let promise = self.ns.put(key, &array).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        let promise = self.ns.delete(key).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)?;
        Ok(())
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        let options = js_sys::Object::new();
        if let Some(p) = prefix {
            js_sys::Reflect::set(&options, &"prefix".into(), &JsValue::from_str(p))
                .map_err(|e| KvError::Backend(format!("{e:?}")))?;
        }

        let promise = self.ns.list(&options).map_err(js_err)?;
        let result = JsFuture::from(promise).await.map_err(js_err)?;

        // Result is { keys: [{name: "key1"}, {name: "key2"}, ...], ... }
        let keys_val = js_sys::Reflect::get(&result, &"keys".into()).map_err(js_err)?;
        let keys_array = js_sys::Array::from(&keys_val);

        let mut keys = Vec::with_capacity(keys_array.length() as usize);
        for i in 0..keys_array.length() {
            let entry = keys_array.get(i);
            let name = js_sys::Reflect::get(&entry, &"name".into())
                .map_err(js_err)?
                .as_string()
                .unwrap_or_default();
            keys.push(name);
        }
        Ok(keys)
    }
}

/// Convert a `JsValue` error to a `KvError`.
///
/// Takes ownership to match `Result<_, JsValue>::map_err` signature.
#[allow(clippy::needless_pass_by_value)]
fn js_err(e: JsValue) -> KvError {
    KvError::Backend(format!("{e:?}"))
}
