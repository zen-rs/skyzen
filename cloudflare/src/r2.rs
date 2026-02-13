//! Cloudflare R2 implementation of [`ObjectStorage`].

use std::collections::HashMap;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use skyzen_services::storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, StorageError, StorageObject,
};

use crate::ffi;

/// A Cloudflare R2 bucket.
///
/// Wraps the R2 bucket binding from the Workers environment.
///
/// # Safety
///
/// WASM in Workers is single-threaded, so `Send` and `Sync` are safe.
pub struct CfR2 {
    bucket: ffi::R2Bucket,
}

impl Clone for CfR2 {
    fn clone(&self) -> Self {
        let js: &JsValue = self.bucket.as_ref();
        Self {
            bucket: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfR2 {}
unsafe impl Sync for CfR2 {}

impl std::fmt::Debug for CfR2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfR2").finish_non_exhaustive()
    }
}

impl CfR2 {
    /// Create a `CfR2` from an R2 bucket binding.
    ///
    /// # Panics
    ///
    /// Panics if the binding is not a valid R2 bucket.
    #[must_use]
    pub fn new(binding: JsValue) -> Self {
        Self {
            bucket: binding.unchecked_into(),
        }
    }

    /// Create a `CfR2` from a Workers env by binding name.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Backend`] if the binding cannot be found.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, StorageError> {
        let binding = ffi::get_binding(env, binding_name).map_err(|e| {
            StorageError::Backend(format!("failed to get R2 binding '{binding_name}': {e:?}"))
        })?;
        Ok(Self::new(binding))
    }
}

/// Extract metadata from an R2 object's HTTP metadata.
fn extract_content_type(http_metadata: &JsValue) -> Option<String> {
    if http_metadata.is_undefined() || http_metadata.is_null() {
        return None;
    }
    js_sys::Reflect::get(http_metadata, &"contentType".into())
        .ok()
        .and_then(|v| v.as_string())
}

/// Extract custom metadata from a JS object into a `HashMap`.
fn extract_custom_metadata(js_metadata: &JsValue) -> HashMap<String, String> {
    if js_metadata.is_undefined() || js_metadata.is_null() {
        return HashMap::new();
    }
    serde_wasm_bindgen::from_value(js_metadata.clone()).unwrap_or_default()
}

impl ObjectStorage for CfR2 {
    async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        let promise = self.bucket.get(key).map_err(js_err)?;
        let result = JsFuture::from(promise).await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }

        let obj: ffi::R2Object = result.unchecked_into();
        let content_type = extract_content_type(&obj.http_metadata());
        let custom = extract_custom_metadata(&obj.custom_metadata());

        let body_promise = obj.array_buffer().map_err(js_err)?;
        let body_buffer = JsFuture::from(body_promise).await.map_err(js_err)?;
        let body = js_sys::Uint8Array::new(&body_buffer).to_vec();

        let metadata = ObjectMetadata {
            key: obj.key(),
            size: f64_to_u64(obj.size()),
            content_type,
            last_modified: None, // R2 does not expose last_modified on get
            metadata: custom,
        };

        Ok(Some(StorageObject { body, metadata }))
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        let array = js_sys::Uint8Array::from(body.as_slice());
        let promise = self.bucket.put(key, &array).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let promise = self.bucket.delete(key).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)?;
        Ok(())
    }

    async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        let js_options = js_sys::Object::new();

        if let Some(ref prefix) = options.prefix {
            js_sys::Reflect::set(&js_options, &"prefix".into(), &JsValue::from_str(prefix))
                .map_err(|e| StorageError::Backend(format!("{e:?}")))?;
        }

        if let Some(limit) = options.limit {
            #[allow(clippy::cast_precision_loss)]
            // JS numbers are f64; usize limit won't exceed 2^53
            js_sys::Reflect::set(
                &js_options,
                &"limit".into(),
                &JsValue::from_f64(limit as f64),
            )
            .map_err(|e| StorageError::Backend(format!("{e:?}")))?;
        }

        if let Some(ref cursor) = options.cursor {
            js_sys::Reflect::set(&js_options, &"cursor".into(), &JsValue::from_str(cursor))
                .map_err(|e| StorageError::Backend(format!("{e:?}")))?;
        }

        let promise = self.bucket.list(&js_options).map_err(js_err)?;
        let result = JsFuture::from(promise).await.map_err(js_err)?;

        // Result is { objects: [...], truncated, cursor }
        let objects_val = js_sys::Reflect::get(&result, &"objects".into()).map_err(js_err)?;
        let objects_array = js_sys::Array::from(&objects_val);

        let mut objects = Vec::with_capacity(objects_array.length() as usize);
        for i in 0..objects_array.length() {
            let entry = objects_array.get(i);
            let key = js_sys::Reflect::get(&entry, &"key".into())
                .map_err(js_err)?
                .as_string()
                .unwrap_or_default();
            let size = f64_to_u64(
                js_sys::Reflect::get(&entry, &"size".into())
                    .map_err(js_err)?
                    .as_f64()
                    .unwrap_or(0.0),
            );

            objects.push(ObjectMetadata {
                key,
                size,
                content_type: None,
                last_modified: None,
                metadata: HashMap::new(),
            });
        }

        let cursor = js_sys::Reflect::get(&result, &"cursor".into())
            .ok()
            .and_then(|v| v.as_string());

        Ok(ListResult { objects, cursor })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        let promise = self.bucket.head(key).map_err(js_err)?;
        let result = JsFuture::from(promise).await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }

        let obj: ffi::R2ObjectHead = result.unchecked_into();
        let content_type = extract_content_type(&obj.http_metadata());
        let custom = extract_custom_metadata(&obj.custom_metadata());

        Ok(Some(ObjectMetadata {
            key: obj.key(),
            size: f64_to_u64(obj.size()),
            content_type,
            last_modified: None,
            metadata: custom,
        }))
    }
}

/// Convert a `JsValue` error to a `StorageError`.
///
/// Takes ownership to match `Result<_, JsValue>::map_err` signature.
#[allow(clippy::needless_pass_by_value)]
fn js_err(e: JsValue) -> StorageError {
    StorageError::Backend(format!("{e:?}"))
}

/// Safely convert a JS f64 size value to u64.
fn f64_to_u64(value: f64) -> u64 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    if value.is_finite() && value >= 0.0 {
        value as u64
    } else {
        0
    }
}
