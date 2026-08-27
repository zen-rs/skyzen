//! Cloudflare KV implementation of [`KeyValueStore`].

use core::time::Duration;

use serde::de::DeserializeOwned;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use worker::send::IntoSendFuture;

use skyzen_services::kv::{KeyValueStore, KvError, KvListOptions, KvListResult};

use crate::ffi;

/// Cloudflare KV imposes a minimum expiration TTL of 60 seconds. The same floor applies to the
/// `cacheTtl` of a read.
const MIN_TTL_SECS: u64 = 60;

/// A Cloudflare Workers KV namespace.
///
/// Wraps the KV namespace binding from the Workers environment.
///
/// # Beyond the portable trait
///
/// [`KeyValueStore`] is the shape every backend shares; three things KV has and that shape cannot
/// express live on the inherent API instead — [`put_with_options`](Self::put_with_options) for
/// absolute expiry and per-value metadata, [`get_with_metadata`](Self::get_with_metadata) to read
/// that metadata back, and [`get_with_cache_ttl`](Self::get_with_cache_ttl) for the colo-local read
/// cache, which is the main KV read-latency lever.
///
/// # Platform limits
///
/// KV is eventually consistent and offers no conditional write, so
/// [`compare_and_swap`](KeyValueStore::compare_and_swap) and
/// [`increment`](KeyValueStore::increment) keep their
/// [`Unsupported`](KvError::Unsupported) defaults: there is no compare-and-set primitive to build
/// them on, and emulating one with read-then-write would silently lose concurrent updates. Reach
/// for a Durable Object when you need atomicity.
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
            KvError::backend(format!("failed to get KV binding '{binding_name}': {e:?}"))
        })?;
        ffi::require_methods(
            &binding,
            binding_name,
            &["get", "getWithMetadata", "put", "delete", "list"],
        )
        .map_err(js_err)?;
        Ok(Self::new(binding))
    }

    /// Store a value with KV's own write options: absolute expiry, relative expiry, and up to
    /// 1 KiB of metadata carried alongside the value.
    ///
    /// Unlike [`put_with_ttl`](KeyValueStore::put_with_ttl), which is the portable call and rounds
    /// a too-short TTL up to the platform floor because the portable signature has no way to
    /// report it, this rejects a TTL under [`MIN_TTL_SECS`] rather than quietly extending it.
    ///
    /// # Errors
    ///
    /// [`KvError::Backend`] when both expiries are set, when a TTL is below the 60 second floor,
    /// when the metadata cannot be encoded, or when the runtime rejects the write.
    pub async fn put_with_options(
        &self,
        key: &str,
        value: &[u8],
        options: CfKvPutOptions,
    ) -> Result<(), KvError> {
        let js_options = options.into_js()?;
        let array = js_sys::Uint8Array::from(value);
        let promise = self.ns.put(key, &array, &js_options).map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    /// Read a value together with the metadata a previous
    /// [`put_with_options`](Self::put_with_options) stored alongside it.
    ///
    /// `Ok(None)` means the key is absent. A present key with no metadata yields a
    /// [`CfKvValueWithMetadata`] whose `metadata` is `None`.
    ///
    /// # Errors
    ///
    /// [`KvError::Backend`] when the runtime rejects the read or the metadata does not deserialize
    /// into `M`.
    pub async fn get_with_metadata<M: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<CfKvValueWithMetadata<M>>, KvError> {
        let options = js_sys::Object::new();
        set_array_buffer_type(&options)?;

        let promise = self.ns.get_with_metadata(key, &options).map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        // The platform always resolves `{ value, metadata }`; `value` is null for a missing key.
        let value = js_sys::Reflect::get(&result, &"value".into()).map_err(js_err)?;
        if value.is_null() || value.is_undefined() {
            return Ok(None);
        }

        let metadata = js_sys::Reflect::get(&result, &"metadata".into()).map_err(js_err)?;
        let metadata = if metadata.is_null() || metadata.is_undefined() {
            None
        } else {
            Some(serde_wasm_bindgen::from_value(metadata).map_err(|error| {
                KvError::backend(format!(
                    "KV metadata for '{key}' did not deserialize: {error}"
                ))
            })?)
        };

        Ok(Some(CfKvValueWithMetadata {
            value: js_sys::Uint8Array::new(&value).to_vec(),
            metadata,
        }))
    }

    /// Read a value, asking the colo to cache it for `cache_ttl`.
    ///
    /// A longer `cacheTtl` trades staleness for latency: a hit in the local colo skips the
    /// round-trip to KV's central store entirely. Cloudflare's floor is 60 seconds, and a shorter
    /// value is rejected rather than rounded up.
    ///
    /// # Errors
    ///
    /// [`KvError::Backend`] when `cache_ttl` is below the 60 second floor or the runtime rejects
    /// the read.
    pub async fn get_with_cache_ttl(
        &self,
        key: &str,
        cache_ttl: Duration,
    ) -> Result<Option<Vec<u8>>, KvError> {
        let seconds = checked_ttl_secs("cacheTtl", cache_ttl)?;

        let options = js_sys::Object::new();
        set_array_buffer_type(&options)?;
        set_number(&options, "cacheTtl", seconds)?;

        let promise = self.ns.get(key, &options).map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }
        Ok(Some(js_sys::Uint8Array::new(&result).to_vec()))
    }
}

/// A KV value read together with the metadata stored alongside it.
#[derive(Debug, Clone)]
pub struct CfKvValueWithMetadata<M> {
    /// The stored bytes.
    pub value: Vec<u8>,
    /// The metadata written with the value, when there was any.
    pub metadata: Option<M>,
}

/// Cloudflare KV's own write options.
///
/// Every field is optional; the default is a plain overwrite that never expires.
#[derive(Debug, Clone, Default)]
pub struct CfKvPutOptions {
    /// Absolute expiry, as seconds since the Unix epoch.
    ///
    /// Mutually exclusive with [`expiration_ttl`](Self::expiration_ttl) — the platform rejects a
    /// write that sets both, so this wrapper does too.
    pub expiration: Option<u64>,
    /// Relative expiry, counted from the moment the write lands.
    pub expiration_ttl: Option<Duration>,
    /// Up to 1 KiB of JSON stored with the value and returned by
    /// [`CfKv::get_with_metadata`], without having to read the value itself.
    pub metadata: Option<serde_json::Value>,
}

impl CfKvPutOptions {
    /// A plain overwrite: no expiry, no metadata.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            expiration: None,
            expiration_ttl: None,
            metadata: None,
        }
    }

    /// Expire the value at an absolute time, in seconds since the Unix epoch.
    #[must_use]
    pub const fn with_expiration(mut self, epoch_seconds: u64) -> Self {
        self.expiration = Some(epoch_seconds);
        self
    }

    /// Expire the value `ttl` after the write lands.
    #[must_use]
    pub const fn with_expiration_ttl(mut self, ttl: Duration) -> Self {
        self.expiration_ttl = Some(ttl);
        self
    }

    /// Store `metadata` alongside the value.
    #[must_use]
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Render the options as the JS object KV's `put` expects.
    fn into_js(self) -> Result<JsValue, KvError> {
        let options = js_sys::Object::new();

        if self.expiration.is_some() && self.expiration_ttl.is_some() {
            return Err(KvError::backend(
                "Cloudflare KV rejects a write that sets both `expiration` and `expirationTtl`; \
                 choose an absolute time or a relative TTL",
            ));
        }

        if let Some(expiration) = self.expiration {
            #[allow(clippy::cast_precision_loss)]
            set_number(&options, "expiration", expiration as f64)?;
        }
        if let Some(ttl) = self.expiration_ttl {
            set_number(
                &options,
                "expirationTtl",
                checked_ttl_secs("expirationTtl", ttl)?,
            )?;
        }
        if let Some(metadata) = self.metadata {
            let value = serde_wasm_bindgen::to_value(&metadata).map_err(|error| {
                KvError::backend(format!("KV metadata did not serialize to JS: {error}"))
            })?;
            js_sys::Reflect::set(&options, &"metadata".into(), &value).map_err(js_err)?;
        }

        Ok(options.into())
    }
}

/// Ask KV for the raw bytes rather than its default UTF-8 decoding.
fn set_array_buffer_type(options: &js_sys::Object) -> Result<(), KvError> {
    js_sys::Reflect::set(options, &"type".into(), &"arrayBuffer".into()).map_err(js_err)?;
    Ok(())
}

fn set_number(options: &js_sys::Object, name: &str, value: f64) -> Result<(), KvError> {
    js_sys::Reflect::set(options, &JsValue::from_str(name), &JsValue::from_f64(value))
        .map_err(js_err)?;
    Ok(())
}

/// Convert a duration to whole seconds, refusing anything under the platform's 60 second floor.
///
/// `field` names the KV option in the error, because the floor applies to two different ones.
fn checked_ttl_secs(field: &str, ttl: Duration) -> Result<f64, KvError> {
    let mut seconds = ttl.as_secs();
    if ttl.subsec_nanos() > 0 {
        seconds = seconds.saturating_add(1);
    }
    if seconds < MIN_TTL_SECS {
        return Err(KvError::backend(format!(
            "Cloudflare KV requires `{field}` to be at least {MIN_TTL_SECS} seconds; got {seconds}"
        )));
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(seconds as f64)
}

impl KeyValueStore for CfKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let options = js_sys::Object::new();
        set_array_buffer_type(&options)?;

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

    /// List one page of keys using Cloudflare KV's own list cursor.
    ///
    /// KV pages at 1000 keys and reports `list_complete` plus a `cursor`; both are handed straight
    /// through, so a caller paginates at the platform's own granularity instead of the worker
    /// buffering an entire namespace inside its 128 MB memory limit.
    async fn list(&self, options: KvListOptions) -> Result<KvListResult, KvError> {
        let js_options = js_sys::Object::new();
        if let Some(prefix) = options.prefix.as_deref() {
            js_sys::Reflect::set(&js_options, &"prefix".into(), &JsValue::from_str(prefix))
                .map_err(js_err)?;
        }
        if let Some(cursor) = options.cursor.as_deref() {
            js_sys::Reflect::set(&js_options, &"cursor".into(), &JsValue::from_str(cursor))
                .map_err(js_err)?;
        }
        if let Some(limit) = options.limit {
            #[allow(clippy::cast_precision_loss)]
            js_sys::Reflect::set(
                &js_options,
                &"limit".into(),
                &JsValue::from_f64(limit as f64),
            )
            .map_err(js_err)?;
        }

        let promise = self.ns.list(&js_options).map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        // Result is { keys: [{name}, ...], list_complete, cursor }
        let keys_val = js_sys::Reflect::get(&result, &"keys".into()).map_err(js_err)?;
        let keys_array = js_sys::Array::from(&keys_val);
        let mut keys = Vec::with_capacity(keys_array.length() as usize);

        for index in 0..keys_array.length() {
            let entry = keys_array.get(index);
            let name = js_sys::Reflect::get(&entry, &"name".into()).map_err(js_err)?;
            let name = name.as_string().ok_or_else(|| {
                KvError::backend(format!("KV list returned a non-string key name: {name:?}"))
            })?;
            keys.push(name);
        }

        let complete = js_sys::Reflect::get(&result, &"list_complete".into())
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        // An incomplete listing without a cursor cannot be resumed; reporting no cursor ends the
        // scan instead of handing the caller a token that would restart it from the beginning.
        let cursor = (!complete)
            .then(|| js_sys::Reflect::get(&result, &"cursor".into()).ok())
            .flatten()
            .and_then(|value| value.as_string());

        Ok(KvListResult { keys, cursor })
    }
}

/// Convert a `JsValue` error to a `KvError`.
///
/// Takes ownership to match `Result<_, JsValue>::map_err` signature.
#[allow(clippy::needless_pass_by_value)]
fn js_err(e: JsValue) -> KvError {
    KvError::backend(format!("{e:?}"))
}
