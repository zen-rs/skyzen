//! Cloudflare R2 implementation of [`ObjectStorage`].
//!
//! # Beyond the portable trait
//!
//! R2 is an object store for large objects, and several of its APIs have no portable equivalent:
//! [`get_if`](CfR2::get_if) for conditional reads,
//! [`delete_many`](CfR2::delete_many) for bulk deletes, and the multipart upload handle
//! ([`create_multipart_upload`](CfR2::create_multipart_upload) /
//! [`resume_multipart_upload`](CfR2::resume_multipart_upload)) for objects past the 5 GiB
//! single-put ceiling. Ranged reads and streaming bodies *do* have portable shapes, so they are
//! implemented as [`ObjectStorage::get_range`], [`ObjectStorage::get_stream`] and
//! [`ObjectStorage::put_stream`] rather than as R2-only methods.
//!
//! # Presigned URLs
//!
//! [`ObjectStorage::presign_get`] and [`ObjectStorage::presign_put`] keep their `Unsupported`
//! defaults. R2 does presign, but only through its S3-compatible endpoint with an account access
//! key — credentials a Worker's bucket binding does not carry and should not. Use
//! [`skyzen_s3`](https://docs.rs/skyzen-s3) against the R2 S3 endpoint when you need one.

use core::{
    pin::Pin,
    task::{Context, Poll},
};
use std::collections::HashMap;

use bytes::Bytes;
use futures_core::Stream;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use worker::send::IntoSendFuture;
use worker_sys::{
    FixedLengthStream, R2Bucket, R2MultipartUpload, R2Object, R2ObjectBody, R2UploadedPart,
};

use skyzen_services::storage::{
    ByteRange, ListOptions, ListResult, ObjectMetadata, ObjectStorage, PutOption, PutOptions,
    StorageError, StorageObject, StorageStream,
};

/// R2 accepts at most 1000 keys in one bulk delete.
const MAX_BULK_DELETE_KEYS: usize = 1000;

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
        let metadata = object_metadata(obj.unchecked_ref())?;
        let body = read_body(&obj).await?;

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
        let js_options = build_put_options(&options)?;
        let array = js_sys::Uint8Array::from(body.as_slice());
        let promise = self
            .bucket
            .put(key.to_owned(), array.into(), js_options)
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

        object_metadata(&result.unchecked_into()).map(Some)
    }

    /// Read part of an object, letting R2 serve only the requested bytes.
    ///
    /// The returned body holds the slice; the metadata keeps reporting the size of the whole
    /// object, so a handler can render a `Content-Range` from both.
    async fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> Result<Option<StorageObject>, StorageError> {
        let options = js_sys::Object::new();
        set(&options, "range", &range_option(range)?.into())?;

        let promise = self
            .bucket
            .get(key.to_owned(), options.into())
            .map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }

        let obj: R2ObjectBody = result.unchecked_into();
        let metadata = object_metadata(obj.unchecked_ref())?;
        let body = read_body(&obj).await?;

        Ok(Some(StorageObject { body, metadata }))
    }

    /// Stream an object's body straight off R2 without buffering it in the isolate.
    ///
    /// A Worker has roughly 128 MB of memory, so this is the only way to serve a large object
    /// through one.
    async fn get_stream(&self, key: &str) -> Result<Option<StorageStream>, StorageError> {
        let promise = self
            .bucket
            .get(key.to_owned(), JsValue::UNDEFINED)
            .map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(None);
        }

        let obj: R2ObjectBody = result.unchecked_into();
        let raw = obj.body().map_err(js_err)?;
        Ok(Some(StorageStream::new(R2BodyStream::new(raw))))
    }

    /// Upload an object from a stream of chunks.
    ///
    /// R2's `put` will not take a `ReadableStream` of unknown length, so `content_length` is
    /// required here: the stream is fed through a `FixedLengthStream` that declares the size up
    /// front. A caller that does not know the size wants
    /// [`create_multipart_upload`](CfR2::create_multipart_upload) instead, which needs no total.
    async fn put_stream(
        &self,
        key: &str,
        stream: StorageStream,
        content_length: Option<u64>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        let Some(content_length) = content_length else {
            return Err(StorageError::Unsupported(
                "R2 requires the length of a streamed upload up front; pass `content_length`, or \
                 use `CfR2::create_multipart_upload` when the total is not known",
            ));
        };
        let js_options = build_put_options(&options)?;

        let length = u32::try_from(content_length).map_err(|_| {
            StorageError::backend(
                "R2 streamed upload length exceeds what a FixedLengthStream can declare; \
                 use `CfR2::create_multipart_upload` for objects this large",
            )
        })?;
        let fixed = FixedLengthStream::new(length).map_err(js_err)?;
        let writable = fixed.writable();
        let readable = fixed.readable();

        let source = wasm_streams::ReadableStream::from_stream(JsChunks(stream)).into_raw();
        let piped = source.pipe_to(&writable);

        let put = self
            .bucket
            .put(key.to_owned(), readable.into(), js_options)
            .map_err(js_err)?;

        // Both halves have to make progress together: the put consumes the readable end while the
        // pipe fills the writable one, so awaiting either alone deadlocks. `Promise.all` also
        // means a failure on either side surfaces instead of becoming an unhandled rejection.
        let both = js_sys::Promise::all(&js_sys::Array::of2(&piped, &put));
        JsFuture::from(both).into_send().await.map_err(js_err)?;
        Ok(())
    }
}

// ── R2-only surface ──

/// The three answers a conditional read can give.
#[derive(Debug)]
pub enum CfR2ConditionalGet {
    /// The precondition held; here is the object.
    Matched(Box<StorageObject>),
    /// The object is there but the precondition did not hold, so R2 served metadata only.
    ///
    /// This is what a `304 Not Modified` is built from, and why the case is distinct from
    /// [`NotFound`](Self::NotFound): reporting it as a miss would turn a cache hit into a 404.
    PreconditionFailed(Box<ObjectMetadata>),
    /// No object under that key.
    NotFound,
}

/// The preconditions R2's `onlyIf` can carry.
///
/// An empty set makes the read unconditional. Combining several is allowed and the platform
/// requires all of them to hold.
#[derive(Debug, Clone, Default)]
pub struct CfR2Conditional {
    /// Serve the object only if its entity tag matches.
    pub etag_matches: Option<String>,
    /// Serve the object only if its entity tag does *not* match — the `If-None-Match` of a
    /// revalidating client.
    pub etag_does_not_match: Option<String>,
    /// Serve the object only if it was uploaded before this instant.
    pub uploaded_before: Option<std::time::SystemTime>,
    /// Serve the object only if it was uploaded after this instant.
    pub uploaded_after: Option<std::time::SystemTime>,
}

impl CfR2Conditional {
    /// A condition set that constrains nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            etag_matches: None,
            etag_does_not_match: None,
            uploaded_before: None,
            uploaded_after: None,
        }
    }

    /// Require the object's entity tag to match.
    #[must_use]
    pub fn with_etag_matches(mut self, etag: impl Into<String>) -> Self {
        self.etag_matches = Some(etag.into());
        self
    }

    /// Require the object's entity tag not to match.
    #[must_use]
    pub fn with_etag_does_not_match(mut self, etag: impl Into<String>) -> Self {
        self.etag_does_not_match = Some(etag.into());
        self
    }

    /// Require the object to have been uploaded before `time`.
    #[must_use]
    pub const fn with_uploaded_before(mut self, time: std::time::SystemTime) -> Self {
        self.uploaded_before = Some(time);
        self
    }

    /// Require the object to have been uploaded after `time`.
    #[must_use]
    pub const fn with_uploaded_after(mut self, time: std::time::SystemTime) -> Self {
        self.uploaded_after = Some(time);
        self
    }

    /// Render the conditions as R2's `onlyIf` object.
    fn into_js(self) -> Result<js_sys::Object, StorageError> {
        let only_if = js_sys::Object::new();
        if let Some(etag) = &self.etag_matches {
            set(&only_if, "etagMatches", &JsValue::from_str(etag))?;
        }
        if let Some(etag) = &self.etag_does_not_match {
            set(&only_if, "etagDoesNotMatch", &JsValue::from_str(etag))?;
        }
        if let Some(time) = self.uploaded_before {
            set(&only_if, "uploadedBefore", to_js_date(time)?.as_ref())?;
        }
        if let Some(time) = self.uploaded_after {
            set(&only_if, "uploadedAfter", to_js_date(time)?.as_ref())?;
        }
        Ok(only_if)
    }
}

impl CfR2 {
    /// Read an object only if the given preconditions hold.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when a timestamp cannot be represented or the runtime rejects the read.
    pub async fn get_if(
        &self,
        key: &str,
        conditional: CfR2Conditional,
    ) -> Result<CfR2ConditionalGet, StorageError> {
        let options = js_sys::Object::new();
        set(&options, "onlyIf", &conditional.into_js()?.into())?;

        let promise = self
            .bucket
            .get(key.to_owned(), options.into())
            .map_err(js_err)?;
        let result = JsFuture::from(promise).into_send().await.map_err(js_err)?;

        if result.is_null() || result.is_undefined() {
            return Ok(CfR2ConditionalGet::NotFound);
        }

        if !has_body(&result) {
            let metadata = object_metadata(&result.unchecked_into())?;
            return Ok(CfR2ConditionalGet::PreconditionFailed(Box::new(metadata)));
        }

        let obj: R2ObjectBody = result.unchecked_into();
        let metadata = object_metadata(obj.unchecked_ref())?;
        let body = read_body(&obj).await?;
        Ok(CfR2ConditionalGet::Matched(Box::new(StorageObject {
            body,
            metadata,
        })))
    }

    /// Delete up to [`MAX_BULK_DELETE_KEYS`] keys in one round trip.
    ///
    /// Deleting a key that does not exist is not an error, matching the single-key
    /// [`delete`](ObjectStorage::delete).
    ///
    /// # Errors
    ///
    /// [`StorageError::Unsupported`] when more than 1000 keys are passed — R2 would reject the
    /// call, and silently splitting it into batches would turn one atomic-looking request into
    /// several that can partly fail. [`StorageError::Backend`] when the runtime rejects the
    /// delete.
    pub async fn delete_many(&self, keys: &[&str]) -> Result<(), StorageError> {
        if keys.len() > MAX_BULK_DELETE_KEYS {
            return Err(StorageError::Unsupported(
                "R2 deletes at most 1000 keys per call; split the request yourself so partial \
                 failures stay visible",
            ));
        }
        if keys.is_empty() {
            return Ok(());
        }

        let js_keys = keys.iter().map(|key| JsValue::from_str(key)).collect();
        let promise = self.bucket.delete_multiple(js_keys).map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    /// Begin a multipart upload, the way to write an object larger than a single put allows.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the runtime rejects the request.
    pub async fn create_multipart_upload(
        &self,
        key: &str,
    ) -> Result<CfR2MultipartUpload, StorageError> {
        self.create_multipart_upload_with(key, PutOptions::new())
            .await
    }

    /// Begin a multipart upload, recording the same metadata a [`put_with`](ObjectStorage::put_with)
    /// would.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when an option cannot be encoded or the runtime rejects the request.
    pub async fn create_multipart_upload_with(
        &self,
        key: &str,
        options: PutOptions,
    ) -> Result<CfR2MultipartUpload, StorageError> {
        let js_options = build_put_options(&options)?;
        let promise = self
            .bucket
            .create_multipart_upload(key.to_owned(), js_options)
            .map_err(js_err)?;
        let upload = JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(CfR2MultipartUpload {
            upload: upload.unchecked_into(),
        })
    }

    /// Re-attach to a multipart upload started earlier, by the id
    /// [`CfR2MultipartUpload::upload_id`] reported.
    ///
    /// A multipart upload outlives the isolate that started it, so this is how a second request
    /// continues one — that is the point of the id being a plain string.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the runtime rejects the request.
    pub fn resume_multipart_upload(
        &self,
        key: &str,
        upload_id: &str,
    ) -> Result<CfR2MultipartUpload, StorageError> {
        let upload = self
            .bucket
            .resume_multipart_upload(key.to_owned(), upload_id.to_owned())
            .map_err(js_err)?;
        Ok(CfR2MultipartUpload {
            upload: upload.unchecked_into(),
        })
    }
}

/// A multipart upload in progress.
///
/// Parts are numbered from 1 and every part except the last must be the same size. Hand the parts
/// [`upload_part`](Self::upload_part) returns to [`complete`](Self::complete), or call
/// [`abort`](Self::abort) to discard the whole upload — an upload left neither completed nor
/// aborted keeps consuming storage.
pub struct CfR2MultipartUpload {
    upload: R2MultipartUpload,
}

impl_js_handle_traits!(CfR2MultipartUpload { upload });

impl CfR2MultipartUpload {
    /// The key this upload will write to.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the runtime rejects the lookup.
    pub fn key(&self) -> Result<String, StorageError> {
        self.upload.key().map_err(js_err)
    }

    /// The upload's identifier, for [`CfR2::resume_multipart_upload`].
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the runtime rejects the lookup.
    pub fn upload_id(&self) -> Result<String, StorageError> {
        self.upload.upload_id().map_err(js_err)
    }

    /// Upload one part, numbered from 1.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the runtime rejects the upload.
    pub async fn upload_part(
        &self,
        part_number: u16,
        body: Vec<u8>,
    ) -> Result<CfR2UploadedPart, StorageError> {
        let array = js_sys::Uint8Array::from(body.as_slice());
        let promise = self
            .upload
            .upload_part(part_number, array.into())
            .map_err(js_err)?;
        let part = JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(CfR2UploadedPart {
            part: part.unchecked_into(),
        })
    }

    /// Assemble the uploaded parts into the final object.
    ///
    /// Consumes the handle: an upload can only be completed once.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the runtime rejects the completion — a missing part or a wrong-sized
    /// one fails here rather than producing a corrupt object.
    pub async fn complete(
        self,
        parts: Vec<CfR2UploadedPart>,
    ) -> Result<ObjectMetadata, StorageError> {
        let js_parts = parts
            .into_iter()
            .map(|part| JsValue::from(part.part))
            .collect();
        let promise = self.upload.complete(js_parts).map_err(js_err)?;
        let object = JsFuture::from(promise).into_send().await.map_err(js_err)?;
        object_metadata(&object.unchecked_into())
    }

    /// Discard the upload and every part already sent.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the runtime rejects the abort.
    pub async fn abort(self) -> Result<(), StorageError> {
        let promise = self.upload.abort().map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }
}

/// A part R2 has accepted, to be handed back to [`CfR2MultipartUpload::complete`].
pub struct CfR2UploadedPart {
    part: R2UploadedPart,
}

impl_js_handle_traits!(CfR2UploadedPart { part });

impl CfR2UploadedPart {
    /// The part's number.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the runtime rejects the lookup.
    pub fn part_number(&self) -> Result<u16, StorageError> {
        self.part.part_number().map_err(js_err)
    }

    /// The part's entity tag.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the runtime rejects the lookup.
    pub fn etag(&self) -> Result<String, StorageError> {
        self.part.etag().map_err(js_err)
    }
}

// ── Streaming adapters ──

/// An R2 body's `ReadableStream`, as the portable chunk stream.
struct R2BodyStream {
    inner: wasm_streams::readable::IntoStream<'static>,
}

impl R2BodyStream {
    fn new(raw: web_sys::ReadableStream) -> Self {
        Self {
            inner: wasm_streams::ReadableStream::from_raw(raw).into_stream(),
        }
    }
}

// SAFETY: Workers WASM runs on one thread, so the JS reader never crosses a thread boundary. The
// portable `StorageStream` requires `Send`, which is what makes this necessary.
unsafe impl Send for R2BodyStream {}
// SAFETY: same single-threaded argument; the stream is only ever polled by the Workers runtime.
unsafe impl Sync for R2BodyStream {}

impl Stream for R2BodyStream {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx).map(|chunk| {
            chunk.map(|chunk| match chunk {
                Ok(value) => js_value_to_bytes(&value),
                Err(error) => Err(StorageError::Io(format!(
                    "R2 body stream read failed: {error:?}"
                ))),
            })
        })
    }
}

/// A portable [`StorageStream`], as the chunk stream a JS `ReadableStream` consumes.
struct JsChunks(StorageStream);

impl Stream for JsChunks {
    type Item = Result<JsValue, JsValue>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx).map(|chunk| {
            chunk.map(|chunk| {
                chunk
                    .map(|bytes| js_sys::Uint8Array::from(bytes.as_ref()).into())
                    .map_err(|error| JsValue::from_str(&error.to_string()))
            })
        })
    }
}

fn js_value_to_bytes(value: &JsValue) -> Result<Bytes, StorageError> {
    if value.is_instance_of::<js_sys::Uint8Array>() || value.is_instance_of::<js_sys::ArrayBuffer>()
    {
        Ok(Bytes::from(js_sys::Uint8Array::new(value).to_vec()))
    } else {
        Err(StorageError::Io(
            "R2 body stream yielded a non-byte chunk".to_owned(),
        ))
    }
}

/// Build the JS options object an R2 `put` (or `createMultipartUpload`) takes.
///
/// R2 honours every extended [`PutOptions`] field, so nothing is refused here — the
/// `reject_unsupported` guard other backends need has nothing to reject.
fn build_put_options(options: &PutOptions) -> Result<JsValue, StorageError> {
    // Named so a future addition to `PutOptions` that R2 does *not* map fails loudly rather than
    // being silently dropped by this builder.
    options.reject_unsupported(&PutOption::ALL)?;

    let js_options = js_sys::Object::new();

    let http_metadata = js_sys::Object::new();
    let mut has_http_metadata = false;
    for (name, value) in [
        ("contentType", options.content_type.as_deref()),
        ("cacheControl", options.cache_control.as_deref()),
        ("contentEncoding", options.content_encoding.as_deref()),
        ("contentDisposition", options.content_disposition.as_deref()),
    ] {
        if let Some(value) = value {
            set(&http_metadata, name, &JsValue::from_str(value))?;
            has_http_metadata = true;
        }
    }
    if has_http_metadata {
        set(&js_options, "httpMetadata", &http_metadata)?;
    }

    if !options.metadata.is_empty() {
        let custom_metadata = js_sys::Object::new();
        for (name, value) in &options.metadata {
            set(&custom_metadata, name, &JsValue::from_str(value))?;
        }
        set(&js_options, "customMetadata", &custom_metadata)?;
    }

    if let Some(storage_class) = &options.storage_class {
        set(
            &js_options,
            "storageClass",
            &JsValue::from_str(storage_class),
        )?;
    }

    if let Some(digest) = &options.content_md5 {
        // R2 verifies the upload against this and rejects a mismatch, which is the whole point of
        // sending it: a truncated body fails the write instead of silently replacing the object.
        set(
            &js_options,
            "md5",
            &js_sys::Uint8Array::from(digest.as_slice()).into(),
        )?;
    }

    Ok(js_options.into())
}

/// `Reflect::set` with the error already mapped.
fn set(object: &js_sys::Object, name: &str, value: &JsValue) -> Result<(), StorageError> {
    js_sys::Reflect::set(object, &JsValue::from_str(name), value).map_err(js_err)?;
    Ok(())
}

/// Render a [`ByteRange`] as R2's `range` option.
fn range_option(range: ByteRange) -> Result<js_sys::Object, StorageError> {
    let js_range = js_sys::Object::new();
    #[allow(clippy::cast_precision_loss)]
    match range {
        ByteRange::FromStart { offset, length } => {
            set(&js_range, "offset", &JsValue::from_f64(offset as f64))?;
            if let Some(length) = length {
                set(&js_range, "length", &JsValue::from_f64(length as f64))?;
            }
        }
        ByteRange::Suffix(length) => {
            set(&js_range, "suffix", &JsValue::from_f64(length as f64))?;
        }
    }
    Ok(js_range)
}

/// Read every metadata field off an R2 object.
fn object_metadata(base: &R2Object) -> Result<ObjectMetadata, StorageError> {
    Ok(ObjectMetadata {
        key: base.key().map_err(js_err)?,
        // R2 reports the size of the whole object here even on a ranged read, where the served
        // slice is described by the separate `range` property — which is exactly what
        // `ObjectMetadata::size` promises, so a handler can build a `Content-Range` from it.
        size: f64_to_u64(base.size().map_err(js_err)?),
        content_type: extract_content_type(base),
        last_modified: extract_last_modified(base),
        metadata: extract_custom_metadata(base),
        etag: extract_etag(base),
        version: base.version().ok(),
    })
}

/// Drain an `R2ObjectBody` into one buffer.
async fn read_body(body: &R2ObjectBody) -> Result<Vec<u8>, StorageError> {
    let promise = body.array_buffer().map_err(js_err)?;
    let buffer = JsFuture::from(promise).into_send().await.map_err(js_err)?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// Whether an R2 get result carries a body.
///
/// R2 answers a conditional get whose precondition failed with a bodyless `R2Object` rather than
/// with null, so "has a body" is the only way to tell a served object from a refused one.
fn has_body(result: &JsValue) -> bool {
    js_sys::Reflect::get(result, &JsValue::from_str("body"))
        .is_ok_and(|body| !body.is_null() && !body.is_undefined())
}

/// Convert an instant to the epoch milliseconds a JS `Date` is built from.
fn to_js_date(time: std::time::SystemTime) -> Result<js_sys::Date, StorageError> {
    let millis = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| {
            StorageError::backend(format!(
                "R2 conditional timestamp predates the epoch: {error}"
            ))
        })?
        .as_millis();
    let millis = u64::try_from(millis).map_err(|_| {
        StorageError::backend("R2 conditional timestamp is too far in the future for a JS Date")
    })?;
    #[allow(clippy::cast_precision_loss)]
    Ok(js_sys::Date::new(&JsValue::from_f64(millis as f64)))
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
