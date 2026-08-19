//! Cloudflare KV implementation of [`KeyValueStore`].

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use worker::send::IntoSendFuture;

use skyzen_services::kv::{KeyValueStore, KvError};

use crate::ffi;

/// Cloudflare KV imposes a minimum expiration TTL of 60 seconds.
const MIN_TTL_SECS: u64 = 60;

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

impl_js_handle_traits!(CfKv { ns });

impl CfKv {
    /// Create a `CfKv` from a KV namespace binding.
    ///
    /// The binding is not validated here; an invalid binding surfaces as a
    /// JS error on first use. Prefer [`CfKv::from_env`], which checks that
    /// the binding looks like a KV namespace.
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
    /// Returns [`KvError::Backend`] if the binding cannot be found or does
    /// not look like a KV namespace.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, KvError> {
        let binding = ffi::get_binding(env, binding_name).map_err(|e| {
            KvError::Backend(format!("failed to get KV binding '{binding_name}': {e:?}"))
        })?;
        ffi::require_methods(&binding, binding_name, &["get", "put", "delete", "list"])
            .map_err(js_err)?;
        Ok(Self::new(binding))
    }
}

impl KeyValueStore for CfKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let options = js_sys::Object::new();
        js_sys::Reflect::set(&options, &"type".into(), &"arrayBuffer".into())
            .map_err(|e| KvError::Backend(format!("{e:?}")))?;

        let promise = self.ns.get(key, &options).map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }

        let array = js_sys::Uint8Array::new(&result);
        Ok(Some(array.to_vec()))
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        let array = js_sys::Uint8Array::from(value);
        let promise = self
            .ns
            .put(key, &array, &JsValue::UNDEFINED)
            .map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    /// Store a value with an expiration TTL.
    ///
    /// Cloudflare KV requires `expirationTtl` to be at least 60 seconds, so
    /// the TTL is rounded up to whole seconds and clamped to a minimum of 60.
    async fn put_with_ttl(
        &self,
        key: &str,
        value: &[u8],
        ttl: core::time::Duration,
    ) -> Result<(), KvError> {
        let mut secs = ttl.as_secs();
        if ttl.subsec_nanos() > 0 {
            secs = secs.saturating_add(1);
        }
        let secs = secs.max(MIN_TTL_SECS);

        let options = js_sys::Object::new();
        #[allow(clippy::cast_precision_loss)]
        js_sys::Reflect::set(
            &options,
            &"expirationTtl".into(),
            &JsValue::from_f64(secs as f64),
        )
        .map_err(js_err)?;

        let array = js_sys::Uint8Array::from(value);
        let promise = self.ns.put(key, &array, &options).map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        let promise = self.ns.delete(key).map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        let mut keys = Vec::new();
        let mut cursor: Option<String> = None;

        // KV list is paginated (1000 keys per page); follow the cursor until
        // the backend reports the listing is complete.
        loop {
            let options = js_sys::Object::new();
            if let Some(p) = prefix {
                js_sys::Reflect::set(&options, &"prefix".into(), &JsValue::from_str(p))
                    .map_err(js_err)?;
            }
            if let Some(c) = &cursor {
                js_sys::Reflect::set(&options, &"cursor".into(), &JsValue::from_str(c))
                    .map_err(js_err)?;
            }

            let promise = self.ns.list(&options).map_err(js_err)?;
            let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

            // Result is { keys: [{name}, ...], list_complete, cursor }
            let keys_val = js_sys::Reflect::get(&result, &"keys".into()).map_err(js_err)?;
            let keys_array = js_sys::Array::from(&keys_val);

            for i in 0..keys_array.length() {
                let entry = keys_array.get(i);
                let name = js_sys::Reflect::get(&entry, &"name".into()).map_err(js_err)?;
                let name = name.as_string().ok_or_else(|| {
                    KvError::Backend(format!("KV list returned a non-string key name: {name:?}"))
                })?;
                keys.push(name);
            }

            let complete = js_sys::Reflect::get(&result, &"list_complete".into())
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if complete {
                break;
            }

            cursor = js_sys::Reflect::get(&result, &"cursor".into())
                .ok()
                .and_then(|v| v.as_string());
            if cursor.is_none() {
                // Defensive: an incomplete listing without a cursor would
                // otherwise loop forever.
                break;
            }
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
