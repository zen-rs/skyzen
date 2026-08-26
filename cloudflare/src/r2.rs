//! Cloudflare R2 implementation of [`ObjectStorage`].

use std::collections::HashMap;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use worker::send::IntoSendFuture;
use worker_sys::{R2Bucket, R2Object, R2ObjectBody};

use skyzen_services::storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, PutOptions, StorageError, StorageObject,
};

/// A Cloudflare R2 bucket.
///
/// Wraps the R2 bucket binding from the Workers environment.
///
/// # Safety
///
/// WASM in Workers is single-threaded, so `Send` and `Sync` are safe.
pub struct CfR2 {
    bucket: R2Bucket,
}

impl_js_handle_traits!(CfR2 { bucket });

impl CfR2 {
    /// Create a `CfR2` from an R2 bucket binding.
    ///
    /// The binding is not validated here; an invalid binding surfaces as a
    /// JS error on first use. Prefer [`CfR2::from_env`], which checks that
    /// the binding looks like an R2 bucket.
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
    /// Returns [`StorageError::Backend`] if the binding cannot be found or
    /// does not look like an R2 bucket.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, StorageError> {
        let binding = crate::ffi::get_binding(env, binding_name).map_err(|e| {
            StorageError::backend(format!("failed to get R2 binding '{binding_name}': {e:?}"))
        })?;
        crate::ffi::require_methods(
            &binding,
            binding_name,
            &["get", "put", "delete", "list", "head"],
        )
        .map_err(js_err)?;
        Ok(Self::new(binding))
    }
}

/// Extract content type from an R2 object's HTTP metadata.
fn extract_content_type(obj: &R2Object) -> Option<String> {
    let http_metadata = obj.http_metadata().ok()?;
    let js: &JsValue = http_metadata.as_ref();
    js_sys::Reflect::get(js, &"contentType".into())
        .ok()
        .and_then(|v| v.as_string())
}

/// Extract custom metadata from an R2 object.
fn extract_custom_metadata(obj: &R2Object) -> HashMap<String, String> {
    obj.custom_metadata()
        .ok()
        .and_then(|m| serde_wasm_bindgen::from_value(m.into()).ok())
        .unwrap_or_default()
}

/// Extract the object's entity tag in the quoted HTTP form.
///
/// R2 exposes both `etag` (a bare hex digest) and `httpEtag` (the same digest quoted, ready for an
/// `ETag` header). [`ObjectMetadata::etag`] is documented as the quoted form, so `httpEtag` is the
/// one that goes there.
fn extract_etag(obj: &R2Object) -> Option<String> {
    obj.http_etag().ok()
}

/// Extract the upload timestamp (seconds since Unix epoch) from an R2 object.
fn extract_last_modified(obj: &R2Object) -> Option<u64> {
    let millis = obj.uploaded().ok()?.get_time();
    if millis.is_finite() && millis >= 0.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some((millis / 1000.0) as u64)
    } else {
        None
    }
}

impl ObjectStorage for CfR2 {
    async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        let promise = self
            .bucket
            .get(key.to_owned(), JsValue::UNDEFINED)
            .map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }

        let obj: R2ObjectBody = result.unchecked_into();
        // R2ObjectBody extends R2Object, so we can upcast to access metadata
        let base: &R2Object = obj.unchecked_ref();
        let content_type = extract_content_type(base);
        let custom = extract_custom_metadata(base);

        let body_promise = obj.array_buffer().map_err(js_err)?;
        let body_buffer = JsFuture::from(body_promise)
            .into_send()
            .await
            .map_err(js_err)?;
        let body = js_sys::Uint8Array::new(&body_buffer).to_vec();

        let metadata = ObjectMetadata {
            key: base.key().map_err(js_err)?,
            size: f64_to_u64(base.size().map_err(js_err)?),
            content_type,
            last_modified: extract_last_modified(base),
            metadata: custom,
            etag: extract_etag(base),
            version: base.version().ok(),
        };

        Ok(Some(StorageObject { body, metadata }))
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        let array = js_sys::Uint8Array::from(body.as_slice());
        let promise = self
            .bucket
            .put(key.to_owned(), array.into(), JsValue::UNDEFINED)
            .map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    async fn put_with(
        &self,
        key: &str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        // Only content type and custom metadata are wired below, so anything further is refused
        // rather than dropped.
        options.reject_unsupported(&[])?;

        let js_options = js_sys::Object::new();

        if let Some(content_type) = &options.content_type {
            let http_metadata = js_sys::Object::new();
            js_sys::Reflect::set(
                &http_metadata,
                &"contentType".into(),
                &JsValue::from_str(content_type),
            )
            .map_err(js_err)?;
            js_sys::Reflect::set(&js_options, &"httpMetadata".into(), &http_metadata)
                .map_err(js_err)?;
        }

        if !options.metadata.is_empty() {
            let custom_metadata = js_sys::Object::new();
            for (name, value) in &options.metadata {
                js_sys::Reflect::set(
                    &custom_metadata,
                    &JsValue::from_str(name),
                    &JsValue::from_str(value),
                )
                .map_err(js_err)?;
            }
            js_sys::Reflect::set(&js_options, &"customMetadata".into(), &custom_metadata)
                .map_err(js_err)?;
        }

        let array = js_sys::Uint8Array::from(body.as_slice());
        let promise = self
            .bucket
            .put(key.to_owned(), array.into(), js_options.into())
            .map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let promise = self.bucket.delete(key.to_owned()).map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        let js_options = js_sys::Object::new();

        if let Some(ref prefix) = options.prefix {
            js_sys::Reflect::set(&js_options, &"prefix".into(), &JsValue::from_str(prefix))
                .map_err(|e| StorageError::backend(format!("{e:?}")))?;
        }

        if let Some(limit) = options.limit {
            #[allow(clippy::cast_precision_loss)]
            // JS numbers are f64; usize limit won't exceed 2^53
            js_sys::Reflect::set(
                &js_options,
                &"limit".into(),
                &JsValue::from_f64(limit as f64),
            )
            .map_err(|e| StorageError::backend(format!("{e:?}")))?;
        }

        if let Some(ref cursor) = options.cursor {
            js_sys::Reflect::set(&js_options, &"cursor".into(), &JsValue::from_str(cursor))
                .map_err(|e| StorageError::backend(format!("{e:?}")))?;
        }

        let promise = self.bucket.list(js_options.into()).map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        // Result is { objects: [...], truncated, cursor }
        let objects_val = js_sys::Reflect::get(&result, &"objects".into()).map_err(js_err)?;
        let objects_array = js_sys::Array::from(&objects_val);

        let mut objects = Vec::with_capacity(objects_array.length() as usize);
        for i in 0..objects_array.length() {
            let entry: R2Object = objects_array.get(i).unchecked_into();

            objects.push(ObjectMetadata {
                key: entry.key().map_err(js_err)?,
                size: f64_to_u64(entry.size().map_err(js_err)?),
                content_type: extract_content_type(&entry),
                last_modified: extract_last_modified(&entry),
                metadata: extract_custom_metadata(&entry),
                etag: extract_etag(&entry),
                version: entry.version().ok(),
            });
        }

        let cursor = js_sys::Reflect::get(&result, &"cursor".into())
            .ok()
            .and_then(|v| v.as_string());

        Ok(ListResult { objects, cursor })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        let promise = self.bucket.head(key.to_owned()).map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }

        let obj: R2Object = result.unchecked_into();
        let content_type = extract_content_type(&obj);
        let custom = extract_custom_metadata(&obj);

        Ok(Some(ObjectMetadata {
            key: obj.key().map_err(js_err)?,
            size: f64_to_u64(obj.size().map_err(js_err)?),
            content_type,
            last_modified: extract_last_modified(&obj),
            metadata: custom,
            etag: extract_etag(&obj),
            version: obj.version().ok(),
        }))
    }
}

/// Convert a `JsValue` error to a `StorageError`.
///
/// Takes ownership to match `Result<_, JsValue>::map_err` signature.
#[allow(clippy::needless_pass_by_value)]
fn js_err(e: JsValue) -> StorageError {
    StorageError::backend(format!("{e:?}"))
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
