//! S3-compatible [`ObjectStorage`] implementation for the Skyzen framework.
//!
//! This crate provides an [`S3Storage`] type that implements the [`ObjectStorage`] trait
//! from `skyzen-services`, enabling any S3-compatible service (AWS S3, `MinIO`, etc.)
//! as an object storage backend.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_s3::S3Storage;
//! use skyzen_services::Storage;
//!
//! let s3 = S3Storage::from_env("my-bucket").await;
//! let storage = Storage::new(s3);
//! storage.put("key.txt", b"hello".to_vec()).await?;
//! ```
//!
//! # Moving large objects
//!
//! Whole-object [`get`](ObjectStorage::get) and [`put`](ObjectStorage::put) hold the body in
//! memory, which is a hard ceiling on a small container and an outright blocker past S3's 5 GiB
//! single-`PUT` limit. Three ways around it are implemented here:
//! [`get_stream`](ObjectStorage::get_stream) and [`put_stream`](ObjectStorage::put_stream) move an
//! object in chunks — `put_stream` switching to a real multipart upload once the body outgrows a
//! single request — [`get_range`](ObjectStorage::get_range) serves an HTTP `Range` without reading
//! the rest, and [`presign_get`](ObjectStorage::presign_get) /
//! [`presign_put`](ObjectStorage::presign_put) hand the client a URL that keeps the transfer off
//! the application server entirely.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::HashMap;

use aws_sdk_s3::config::Builder as S3ConfigBuilder;
use aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, StorageClass};
use aws_sdk_s3::Client;
use aws_smithy_types::error::display::DisplayErrorContext;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use bytes::Bytes;
use futures_core::Stream;
use http_kit::header::{HeaderName, HeaderValue};
use http_kit::Method;
use skyzen_services::storage::{
    ByteRange, ListOptions, ListResult, ObjectMetadata, ObjectStorage, PresignedRequest, PutOption,
    PutOptions, StorageError, StorageObject, StorageStream,
};

/// Above this many bytes a streamed upload becomes a multipart upload.
///
/// A single `PutObject` is one request and one signature; multipart costs at least three requests
/// plus one per part. S3 accepts a single put up to 5 GiB, so this threshold is a memory budget
/// rather than a platform limit: it is how much of an unsized stream this backend is willing to
/// hold in RAM to find out whether the extra round trips are needed at all.
const SINGLE_PUT_THRESHOLD: usize = 16 * 1024 * 1024;

/// The target size of one multipart part.
///
/// S3 requires every part but the last to be at least 5 MiB and accepts at most
/// [`MAX_MULTIPART_PARTS`], so 8 MiB parts carry an object of about 78 GiB while holding only one
/// part in memory at a time.
const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

/// How many parts S3 accepts in one multipart upload.
const MAX_MULTIPART_PARTS: i32 = 10_000;

/// Extended [`PutOptions`] a single `PutObject` records: S3 has a header for every one.
const SINGLE_PUT_OPTIONS: [PutOption; 5] = PutOption::ALL;

/// Extended [`PutOptions`] a multipart upload records.
///
/// Everything except the checksum. A multipart object's `ETag` is derived from the part digests
/// rather than from the whole body, so there is nothing for a whole-object MD5 to verify against
/// and `CreateMultipartUpload` has no `Content-MD5` field. Refusing the option is better than
/// accepting an upload whose integrity was silently never checked.
const MULTIPART_PUT_OPTIONS: [PutOption; 4] = [
    PutOption::CacheControl,
    PutOption::ContentEncoding,
    PutOption::ContentDisposition,
    PutOption::StorageClass,
];

/// Codes S3 uses to say the caller is over a request-rate limit.
const THROTTLING_CODES: [&str; 2] = ["SlowDown", "RequestLimitExceeded"];

/// Codes S3 uses to reject the request's credentials or permissions.
const UNAUTHORIZED_CODES: [&str; 4] = [
    "AccessDenied",
    "AllAccessDisabled",
    "InvalidAccessKeyId",
    "SignatureDoesNotMatch",
];

/// An S3-compatible object storage backend.
///
/// Supports any S3-compatible service including AWS S3, `MinIO`, Cloudflare R2,
/// and other compatible providers via custom endpoint configuration.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Debug, Clone)]
pub struct S3Storage {
    client: Client,
    bucket: String,
}

impl S3Storage {
    /// Create a new `S3Storage` from an existing [`Client`] and bucket name.
    pub fn new(client: Client, bucket: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
        }
    }

    /// Create a new `S3Storage` from environment configuration.
    ///
    /// Uses the default AWS SDK configuration loader, which reads
    /// from environment variables, config files, and instance metadata.
    pub async fn from_env(bucket: impl Into<String>) -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = Client::new(&config);
        Self::new(client, bucket)
    }

    /// Create a new `S3Storage` with a custom endpoint URL.
    ///
    /// Use this for S3-compatible services like `MinIO` or local development.
    pub async fn with_endpoint(bucket: impl Into<String>, endpoint_url: impl Into<String>) -> Self {
        let shared_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let s3_config = S3ConfigBuilder::from(&shared_config)
            .endpoint_url(endpoint_url)
            .force_path_style(true)
            .build();
        let client = Client::from_conf(s3_config);
        Self::new(client, bucket)
    }

    /// Upload a body S3 will not take in one request, as a multipart upload.
    ///
    /// `first_part` is what [`ObjectStorage::put_stream`] already read looking for the end of the
    /// stream, so it becomes part 1 rather than being buffered twice.
    async fn multipart_upload(
        &self,
        key: &str,
        first_part: Vec<u8>,
        stream: StorageStream,
        content_length: Option<u64>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        options.reject_unsupported(&MULTIPART_PUT_OPTIONS)?;

        let created = apply_put_options(
            self.client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(key),
            &options,
        )?
        .send()
        .await
        .map_err(sdk_error)?;

        let Some(upload_id) = created.upload_id() else {
            return Err(StorageError::backend(format!(
                "S3 started a multipart upload of {key:?} without returning an upload id"
            )));
        };

        let result = self
            .run_multipart(key, upload_id, first_part, stream, content_length)
            .await;

        if let Err(error) = result {
            // Parts already uploaded keep being billed until the upload is aborted or a bucket
            // lifecycle rule sweeps it, so the abort runs on *every* failure path — a part that
            // failed to upload, a length that did not match, and a completion that was rejected.
            if let Err(abort_error) = self.abort_multipart(key, upload_id).await {
                tracing::error!(
                    key,
                    upload_id,
                    error = %abort_error,
                    "could not abort the failed multipart upload; its parts stay billed until a \
                     bucket lifecycle rule removes them"
                );
            }
            return Err(error);
        }

        Ok(())
    }

    /// Upload every part and complete the upload, leaving the abort to the caller.
    async fn run_multipart(
        &self,
        key: &str,
        upload_id: &str,
        first_part: Vec<u8>,
        mut stream: StorageStream,
        content_length: Option<u64>,
    ) -> Result<(), StorageError> {
        let mut parts = Vec::new();
        let mut part_number: i32 = 1;
        let mut body = first_part;
        let mut uploaded: u64 = 0;

        loop {
            uploaded = uploaded.saturating_add(body.len() as u64);

            let output = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(body))
                .send()
                .await
                .map_err(sdk_error)?;

            let Some(e_tag) = output.e_tag() else {
                return Err(StorageError::backend(format!(
                    "S3 accepted part {part_number} of {key:?} without an ETag, so the upload \
                     cannot be completed"
                )));
            };
            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(e_tag)
                    .build(),
            );

            // `take_chunk` only returns an empty buffer when the stream ended, so this is the
            // stream's end rather than a short read.
            let (next, _) = take_chunk(&mut stream, MULTIPART_PART_SIZE).await?;
            if next.is_empty() {
                break;
            }

            part_number = part_number
                .checked_add(1)
                .filter(|number| *number <= MAX_MULTIPART_PARTS)
                .ok_or_else(|| {
                    StorageError::backend(format!(
                        "streamed upload of {key:?} needs more than the {MAX_MULTIPART_PARTS} \
                         parts S3 accepts; use a larger part size or a smaller object"
                    ))
                })?;
            body = next;
        }

        if let Some(declared) = content_length {
            if declared != uploaded {
                return Err(StorageError::backend(format!(
                    "streamed upload of {key:?} declared {declared} bytes but the stream yielded \
                     {uploaded}"
                )));
            }
        }

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(sdk_error)?;

        Ok(())
    }

    /// Discard a multipart upload and the parts already stored for it.
    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<(), StorageError> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }
}

/// Convert an S3 `DateTime` to seconds since Unix epoch.
fn timestamp_secs(dt: &aws_smithy_types::DateTime) -> Option<u64> {
    u64::try_from(dt.secs()).ok()
}

/// Convert an S3 i64 size to u64, clamping negatives to 0.
fn size_u64(size: i64) -> u64 {
    u64::try_from(size).unwrap_or(0)
}

/// Map an AWS SDK error to a [`StorageError`], reading its service error code first.
///
/// A `SlowDown` is a rate limit the caller should back off on and an `AccessDenied` is a permission
/// problem retrying can never fix; collapsing both into one `Backend` message would leave a handler
/// substring-matching to tell them apart. Everything else keeps the full message, built through
/// [`DisplayErrorContext`] so it walks the whole error source chain and carries the service error
/// code instead of just "service error".
fn sdk_error<E>(err: E) -> StorageError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    match err.code() {
        Some(code) if THROTTLING_CODES.contains(&code) => {
            StorageError::Throttled { retry_after: None }
        }
        Some(code) if UNAUTHORIZED_CODES.contains(&code) => StorageError::Unauthorized,
        _ => StorageError::backend_with(DisplayErrorContext(&err).to_string(), err),
    }
}

/// The subset of a request builder that records [`PutOptions`].
///
/// `PutObject` and `CreateMultipartUpload` take the same object metadata through separately
/// generated builder types. Writing the wiring once against this trait is what stops a single-shot
/// put and a multipart upload from drifting into recording different metadata for the same call.
trait PutRequest: Sized {
    /// Record the object's MIME type.
    fn with_content_type(self, value: String) -> Self;
    /// Record the object's custom metadata.
    fn with_metadata(self, value: HashMap<String, String>) -> Self;
    /// Record the `Cache-Control` the object is served with.
    fn with_cache_control(self, value: String) -> Self;
    /// Record the `Content-Encoding` the object is served with.
    fn with_content_encoding(self, value: String) -> Self;
    /// Record the `Content-Disposition` the object is served with.
    fn with_content_disposition(self, value: String) -> Self;
    /// Record the object's storage tier.
    fn with_storage_class(self, value: StorageClass) -> Self;
}

/// Implement [`PutRequest`] for builders that already carry the matching inherent setters.
macro_rules! impl_put_request {
    ($($builder:ty),+ $(,)?) => {$(
        impl PutRequest for $builder {
            fn with_content_type(self, value: String) -> Self {
                self.content_type(value)
            }
            fn with_metadata(self, value: HashMap<String, String>) -> Self {
                self.set_metadata(Some(value))
            }
            fn with_cache_control(self, value: String) -> Self {
                self.cache_control(value)
            }
            fn with_content_encoding(self, value: String) -> Self {
                self.content_encoding(value)
            }
            fn with_content_disposition(self, value: String) -> Self {
                self.content_disposition(value)
            }
            fn with_storage_class(self, value: StorageClass) -> Self {
                self.storage_class(value)
            }
        }
    )+};
}

impl_put_request!(PutObjectFluentBuilder, CreateMultipartUploadFluentBuilder);

/// Record every [`PutOptions`] field the request type carries.
///
/// [`PutOptions::content_md5`] is deliberately absent: only `PutObject` has a field for it, so it
/// is wired at that one call site rather than being pushed into the shared trait as an option half
/// the implementors would have to refuse.
fn apply_put_options<R: PutRequest>(
    mut request: R,
    options: &PutOptions,
) -> Result<R, StorageError> {
    if let Some(content_type) = &options.content_type {
        request = request.with_content_type(content_type.clone());
    }
    if !options.metadata.is_empty() {
        request = request.with_metadata(options.metadata.clone());
    }
    if let Some(cache_control) = &options.cache_control {
        request = request.with_cache_control(cache_control.clone());
    }
    if let Some(content_encoding) = &options.content_encoding {
        request = request.with_content_encoding(content_encoding.clone());
    }
    if let Some(content_disposition) = &options.content_disposition {
        request = request.with_content_disposition(content_disposition.clone());
    }
    if let Some(name) = &options.storage_class {
        request = request.with_storage_class(storage_class(name)?);
    }
    Ok(request)
}

/// Parse a [`PutOptions::storage_class`] name into S3's own enum.
///
/// The generated `From<&str>` never fails — an unrecognised name becomes an opaque `Unknown`
/// variant S3 rejects on the wire — so the name is checked against the SDK's own list first and a
/// typo is reported here, naming what S3 does accept.
fn storage_class(name: &str) -> Result<StorageClass, StorageError> {
    if StorageClass::values().contains(&name) {
        Ok(StorageClass::from(name))
    } else {
        Err(StorageError::backend(format!(
            "{name:?} is not an S3 storage class; S3 accepts {}",
            StorageClass::values().join(", ")
        )))
    }
}

/// Encode a raw MD5 digest the way S3's `Content-MD5` header wants it.
///
/// [`PutOptions::content_md5`] carries the raw digest bytes precisely so each backend can encode
/// them its own way; S3 wants base64, not hex.
fn content_md5_header(digest: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// Render a [`ByteRange`] as an HTTP `Range` header value.
///
/// S3 takes the header verbatim and both forms map straight across, but HTTP ranges are inclusive
/// at *both* ends while [`ByteRange::FromStart`] carries a length, so the last byte is one before
/// the end. A range that selects nothing is refused rather than sent: S3 would answer it with an
/// `InvalidRange` error, and the caller learns more from the message here.
///
/// # Errors
///
/// [`StorageError::Backend`] for a zero-length range, which no object can satisfy.
fn range_header(range: ByteRange) -> Result<String, StorageError> {
    let empty = || {
        StorageError::backend(format!(
            "byte range {range:?} selects no bytes, which no object can satisfy"
        ))
    };

    match range {
        ByteRange::FromStart {
            offset,
            length: None,
        } => Ok(format!("bytes={offset}-")),
        ByteRange::FromStart {
            offset,
            length: Some(length),
        } => {
            let last = offset
                .checked_add(length)
                .and_then(|end| end.checked_sub(1))
                .filter(|last| *last >= offset)
                .ok_or_else(empty)?;
            Ok(format!("bytes={offset}-{last}"))
        }
        ByteRange::Suffix(0) => Err(empty()),
        ByteRange::Suffix(length) => Ok(format!("bytes=-{length}")),
    }
}

/// Read the whole object's size out of a `Content-Range` response header.
///
/// S3 answers a satisfiable range with `Content-Range: bytes <first>-<last>/<total>`, and `<total>`
/// is the only place the whole object's size appears — `Content-Length` covers the slice.
/// [`ObjectMetadata::size`] is documented as the whole object even on a ranged read, so that is
/// what a handler builds its own `Content-Range` from.
fn total_from_content_range(content_range: &str) -> Option<u64> {
    content_range.rsplit_once('/')?.1.trim().parse().ok()
}

/// Pull the next chunk out of a [`StorageStream`].
async fn next_chunk(stream: &mut StorageStream) -> Option<Result<Bytes, StorageError>> {
    core::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// Drain `stream` into a buffer until it holds at least `target` bytes or the stream ends.
///
/// Returns the buffer and whether the stream is exhausted. The buffer can overshoot `target` by up
/// to one chunk — chunks are not split, and S3 accepts a part of any size up to 5 GiB — but never
/// comes back empty unless the stream really has ended, which is what lets the caller use an empty
/// buffer as the end marker.
async fn take_chunk(
    stream: &mut StorageStream,
    target: usize,
) -> Result<(Vec<u8>, bool), StorageError> {
    let mut buffer = Vec::new();
    while buffer.len() < target {
        match next_chunk(stream).await {
            None => return Ok((buffer, true)),
            Some(chunk) => buffer.extend_from_slice(&chunk?),
        }
    }
    Ok((buffer, false))
}

/// Adapts an S3 [`ByteStream`] to the item type the portable [`StorageStream`] carries.
///
/// Boxed rather than pin-projected so the projection needs no `unsafe`; the extra allocation is
/// once per object read.
struct S3BodyStream(Pin<Box<ByteStream>>);

impl Stream for S3BodyStream {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0
            .as_mut()
            .poll_next(cx)
            .map_err(|error| StorageError::Io(error.to_string()))
    }
}

/// Build the SDK's presigning config, naming `SigV4`'s own ceiling when the request exceeds it.
fn presigning_config(expires_in: core::time::Duration) -> Result<PresigningConfig, StorageError> {
    PresigningConfig::expires_in(expires_in).map_err(|error| {
        StorageError::backend_with(
            format!(
                "S3 cannot presign a request valid for {expires_in:?}; SigV4 caps a presigned URL \
                 at one week"
            ),
            error,
        )
    })
}

/// Convert the SDK's presigned request into the portable [`PresignedRequest`].
///
/// The signature covers the method and the listed headers, so both travel with the URL: a client
/// that changes either invalidates it.
fn presigned_request(
    presigned: &aws_sdk_s3::presigning::PresignedRequest,
) -> Result<PresignedRequest, StorageError> {
    let method = Method::from_bytes(presigned.method().as_bytes()).map_err(|error| {
        StorageError::backend_with(
            format!(
                "S3 presigned a request for method {:?}, which is not an HTTP method",
                presigned.method()
            ),
            error,
        )
    })?;

    let headers = presigned
        .headers()
        .map(|(name, value)| {
            let name = HeaderName::try_from(name).map_err(|error| {
                StorageError::backend_with(
                    format!("S3 presigned a request with header name {name:?}"),
                    error,
                )
            })?;
            let value = HeaderValue::try_from(value).map_err(|error| {
                StorageError::backend_with(
                    format!("S3 presigned a request with an unrepresentable {name} header"),
                    error,
                )
            })?;
            Ok((name, value))
        })
        .collect::<Result<Vec<_>, StorageError>>()?;

    Ok(PresignedRequest {
        url: presigned.uri().to_owned(),
        method,
        headers,
    })
}

impl ObjectStorage for S3Storage {
    async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;

        match result {
            Ok(output) => {
                let content_type = output.content_type().map(ToOwned::to_owned);
                let last_modified = output.last_modified().and_then(timestamp_secs);
                let user_metadata = output.metadata().cloned().unwrap_or_default();
                let etag = output.e_tag().map(ToOwned::to_owned);
                let version = output.version_id().map(ToOwned::to_owned);

                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| StorageError::Io(e.to_string()))?
                    .to_vec();

                let metadata = ObjectMetadata {
                    key: key.to_owned(),
                    size: body.len() as u64,
                    content_type,
                    last_modified,
                    metadata: user_metadata,
                    etag,
                    version,
                };

                Ok(Some(StorageObject { body, metadata }))
            }
            Err(err) => {
                // Only a typed NoSuchKey means "key absent" — a missing bucket
                // (NoSuchBucket) is also a 404 but must surface as an error.
                if err
                    .as_service_error()
                    .is_some_and(GetObjectError::is_no_such_key)
                {
                    Ok(None)
                } else {
                    Err(sdk_error(err))
                }
            }
        }
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    /// Store an object in one request, recording every [`PutOptions`] field.
    ///
    /// S3 has a header for all of them, so nothing is refused here — the
    /// [`reject_unsupported`](PutOptions::reject_unsupported) call is what makes a *future*
    /// addition to `PutOptions` that this backend does not map fail loudly instead of being
    /// dropped on the way to the wire.
    async fn put_with(
        &self,
        key: &str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        options.reject_unsupported(&SINGLE_PUT_OPTIONS)?;

        let mut request = apply_put_options(
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(ByteStream::from(body)),
            &options,
        )?;

        if let Some(digest) = &options.content_md5 {
            request = request.content_md5(content_md5_header(digest));
        }

        request.send().await.map_err(sdk_error)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    /// List one page of objects using `ListObjectsV2`'s own continuation token.
    ///
    /// [`ObjectMetadata::content_type`] is always `None` here, and
    /// [`ObjectMetadata::metadata`] always empty: `ListObjectsV2` reports a key, a size, a
    /// modification time and an `ETag`, and nothing else. Filling them in would cost one `HeadObject`
    /// per key — turning a single listing request into hundreds — so the listing reports what S3
    /// actually returned and a caller that needs the content type of a specific object asks
    /// [`head`](ObjectStorage::head) for it.
    async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        let mut request = self.client.list_objects_v2().bucket(&self.bucket);

        if let Some(ref prefix) = options.prefix {
            request = request.prefix(prefix);
        }

        if let Some(limit) = options.limit {
            request = request.max_keys(i32::try_from(limit).unwrap_or(i32::MAX));
        }

        if let Some(ref cursor) = options.cursor {
            request = request.continuation_token(cursor);
        }

        let output = request.send().await.map_err(sdk_error)?;

        let objects = output
            .contents()
            .iter()
            .map(|obj| ObjectMetadata {
                key: obj.key().unwrap_or_default().to_owned(),
                size: size_u64(obj.size().unwrap_or_default()),
                // ListObjectsV2 does not report either; see this method's documentation.
                content_type: None,
                last_modified: obj.last_modified().and_then(timestamp_secs),
                metadata: HashMap::new(),
                etag: obj.e_tag().map(ToOwned::to_owned),
                // ListObjectsV2 reports the current version only, without naming it.
                version: None,
            })
            .collect();

        let cursor = output.next_continuation_token().map(ToOwned::to_owned);

        Ok(ListResult { objects, cursor })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;

        match result {
            Ok(output) => {
                let content_type = output.content_type().map(ToOwned::to_owned);
                let last_modified = output.last_modified().and_then(timestamp_secs);
                let size = size_u64(output.content_length().unwrap_or_default());

                Ok(Some(ObjectMetadata {
                    key: key.to_owned(),
                    size,
                    content_type,
                    last_modified,
                    metadata: output.metadata().cloned().unwrap_or_default(),
                    etag: output.e_tag().map(ToOwned::to_owned),
                    version: output.version_id().map(ToOwned::to_owned),
                }))
            }
            Err(err) => {
                // Only a typed NotFound means "key absent"; other errors
                // (including a missing bucket) surface as backend errors.
                if err
                    .as_service_error()
                    .is_some_and(HeadObjectError::is_not_found)
                {
                    Ok(None)
                } else {
                    Err(sdk_error(err))
                }
            }
        }
    }

    /// Stream an object's body straight off S3 without buffering it.
    ///
    /// The SDK's `ByteStream` is already chunked, so this hands the response body through rather
    /// than collecting it — the difference between serving a 2 GB object and running out of memory.
    async fn get_stream(&self, key: &str) -> Result<Option<StorageStream>, StorageError> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;

        match result {
            Ok(output) => Ok(Some(StorageStream::new(S3BodyStream(Box::pin(
                output.body,
            ))))),
            Err(err) => {
                if err
                    .as_service_error()
                    .is_some_and(GetObjectError::is_no_such_key)
                {
                    Ok(None)
                } else {
                    Err(sdk_error(err))
                }
            }
        }
    }

    /// Upload from a stream, switching to a multipart upload once the body outgrows one request.
    ///
    /// The stream is read up to [`SINGLE_PUT_THRESHOLD`] first, whatever `content_length` claims.
    /// A stream that ends inside it — which includes every empty one, and every short one whose
    /// length was not known up front — goes out as a single `PutObject`; only a body that really
    /// is large pays for `CreateMultipartUpload`, and `CompleteMultipartUpload` is never asked to
    /// complete zero parts, which S3 rejects.
    ///
    /// A `content_length` that disagrees with what the stream actually yielded fails the upload
    /// rather than storing a truncated or over-long object.
    async fn put_stream(
        &self,
        key: &str,
        mut stream: StorageStream,
        content_length: Option<u64>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        let (head, exhausted) = take_chunk(&mut stream, SINGLE_PUT_THRESHOLD).await?;

        if exhausted {
            if let Some(declared) = content_length {
                if declared != head.len() as u64 {
                    return Err(StorageError::backend(format!(
                        "streamed upload of {key:?} declared {declared} bytes but the stream \
                         yielded {}",
                        head.len()
                    )));
                }
            }
            return self.put_with(key, head, options).await;
        }

        self.multipart_upload(key, head, stream, content_length, options)
            .await
    }

    /// Read part of an object, letting S3 serve only the requested bytes.
    ///
    /// The returned body holds the slice; the metadata keeps reporting the size of the whole
    /// object, read out of the `Content-Range` S3 answers with, so a handler can render its own
    /// `Content-Range` from both.
    async fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> Result<Option<StorageObject>, StorageError> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(range_header(range)?)
            .send()
            .await;

        match result {
            Ok(output) => {
                let content_type = output.content_type().map(ToOwned::to_owned);
                let last_modified = output.last_modified().and_then(timestamp_secs);
                let user_metadata = output.metadata().cloned().unwrap_or_default();
                let etag = output.e_tag().map(ToOwned::to_owned);
                let version = output.version_id().map(ToOwned::to_owned);
                let content_range = output.content_range().map(ToOwned::to_owned);

                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| StorageError::Io(e.to_string()))?
                    .to_vec();

                // A response carrying no `Content-Range` is a plain 200 rather than a 206, and a
                // 200 means the body *is* the whole object.
                let size = match &content_range {
                    None => body.len() as u64,
                    Some(content_range) => {
                        total_from_content_range(content_range).ok_or_else(|| {
                            StorageError::backend(format!(
                                "S3 answered a ranged read of {key:?} with an unparsable \
                                 Content-Range {content_range:?}"
                            ))
                        })?
                    }
                };

                Ok(Some(StorageObject {
                    body,
                    metadata: ObjectMetadata {
                        key: key.to_owned(),
                        size,
                        content_type,
                        last_modified,
                        metadata: user_metadata,
                        etag,
                        version,
                    },
                }))
            }
            Err(err) => {
                if err
                    .as_service_error()
                    .is_some_and(GetObjectError::is_no_such_key)
                {
                    Ok(None)
                } else {
                    Err(sdk_error(err))
                }
            }
        }
    }

    /// Mint a presigned `GET` a browser can follow to download the object directly.
    ///
    /// Signing is local: no request reaches S3, and the URL is valid whether or not the object
    /// exists yet.
    async fn presign_get(
        &self,
        key: &str,
        expires_in: core::time::Duration,
    ) -> Result<PresignedRequest, StorageError> {
        let presigned = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presigning_config(expires_in)?)
            .await
            .map_err(sdk_error)?;

        presigned_request(&presigned)
    }

    /// Mint a presigned `PUT` a browser can upload to directly, keeping a large upload off the
    /// application server entirely.
    ///
    /// Any [`PutOptions`] set here are baked into the signature, so the client must send the
    /// matching headers — which is exactly what [`PresignedRequest::headers`] carries.
    async fn presign_put(
        &self,
        key: &str,
        expires_in: core::time::Duration,
        options: PutOptions,
    ) -> Result<PresignedRequest, StorageError> {
        options.reject_unsupported(&SINGLE_PUT_OPTIONS)?;

        let mut request = apply_put_options(
            self.client.put_object().bucket(&self.bucket).key(key),
            &options,
        )?;

        if let Some(digest) = &options.content_md5 {
            request = request.content_md5(content_md5_header(digest));
        }

        let presigned = request
            .presigned(presigning_config(expires_in)?)
            .await
            .map_err(sdk_error)?;

        presigned_request(&presigned)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        content_md5_header, range_header, size_u64, storage_class, take_chunk, timestamp_secs,
        total_from_content_range, S3Storage, MAX_MULTIPART_PARTS, MULTIPART_PART_SIZE,
        MULTIPART_PUT_OPTIONS, SINGLE_PUT_OPTIONS, SINGLE_PUT_THRESHOLD,
    };
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
    use aws_sdk_s3::{Client, Config};
    use aws_smithy_types::DateTime;
    use bytes::Bytes;
    use core::pin::Pin;
    use core::task::{Context, Poll};
    use futures_core::Stream;
    use skyzen_services::storage::{
        ByteRange, ObjectStorage, PutOption, PutOptions, StorageError, StorageStream,
    };

    /// A stream that hands back a fixed sequence of chunks, one per poll.
    struct Chunks(Vec<Bytes>);

    impl Stream for Chunks {
        type Item = Result<Bytes, StorageError>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let chunks = &mut self.get_mut().0;
            Poll::Ready((!chunks.is_empty()).then(|| Ok(chunks.remove(0))))
        }
    }

    fn stream_of(chunks: &[&[u8]]) -> StorageStream {
        StorageStream::new(Chunks(
            chunks
                .iter()
                .map(|chunk| Bytes::from(chunk.to_vec()))
                .collect(),
        ))
    }

    /// A client with fixed credentials; presigning is local, so no request is ever issued.
    fn storage() -> S3Storage {
        S3Storage::new(
            Client::from_conf(
                Config::builder()
                    .behavior_version(BehaviorVersion::latest())
                    .region(Region::new("us-east-1"))
                    .credentials_provider(Credentials::new(
                        "AKIDTEST", "secret", None, None, "tests",
                    ))
                    .build(),
            ),
            "skyzen-tests",
        )
    }

    #[test]
    fn timestamp_secs_converts_positive_epochs() {
        assert_eq!(
            timestamp_secs(&DateTime::from_secs(1_700_000_000)),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn timestamp_secs_rejects_pre_epoch_dates() {
        assert_eq!(timestamp_secs(&DateTime::from_secs(-1)), None);
    }

    #[test]
    fn size_u64_clamps_negative_sizes_to_zero() {
        assert_eq!(size_u64(-5), 0);
        assert_eq!(size_u64(0), 0);
        assert_eq!(size_u64(42), 42);
    }

    #[test]
    fn range_header_renders_both_forms_with_inclusive_bounds() {
        // HTTP ranges are inclusive at both ends, so 3 bytes from offset 2 is `2-4`, not `2-5`.
        assert_eq!(range_header(ByteRange::slice(2, 3)).unwrap(), "bytes=2-4");
        assert_eq!(range_header(ByteRange::slice(0, 1)).unwrap(), "bytes=0-0");
        assert_eq!(range_header(ByteRange::from_start(4)).unwrap(), "bytes=4-");
        assert_eq!(range_header(ByteRange::from_start(0)).unwrap(), "bytes=0-");
        assert_eq!(range_header(ByteRange::suffix(500)).unwrap(), "bytes=-500");
    }

    #[test]
    fn range_header_refuses_a_range_that_selects_nothing() {
        assert!(range_header(ByteRange::slice(3, 0)).is_err());
        assert!(range_header(ByteRange::suffix(0)).is_err());
        // An offset plus length that overflows cannot name a last byte either.
        assert!(range_header(ByteRange::slice(u64::MAX, u64::MAX)).is_err());
    }

    #[test]
    fn content_range_reports_the_whole_object_not_the_slice() {
        assert_eq!(total_from_content_range("bytes 0-9/12345"), Some(12_345));
        assert_eq!(total_from_content_range("bytes 200-1000/1001"), Some(1001));
        // S3 writes `*` when it will not name a total, which is not a size.
        assert_eq!(total_from_content_range("bytes 0-9/*"), None);
        assert_eq!(total_from_content_range("nonsense"), None);
    }

    #[tokio::test]
    async fn take_chunk_stops_at_the_target_and_reports_the_end_of_the_stream() {
        let mut stream = stream_of(&[b"abc", b"def", b"ghi"]);

        let (first, exhausted) = take_chunk(&mut stream, 4).await.unwrap();
        // Chunks are never split, so a read can overshoot its target by up to one chunk.
        assert_eq!(first, b"abcdef".to_vec());
        assert!(!exhausted);

        let (second, exhausted) = take_chunk(&mut stream, 4).await.unwrap();
        assert_eq!(second, b"ghi".to_vec());
        assert!(exhausted);

        // An exhausted stream comes back empty, which is what marks the end for the part loop.
        let (third, exhausted) = take_chunk(&mut stream, 4).await.unwrap();
        assert!(third.is_empty());
        assert!(exhausted);
    }

    #[tokio::test]
    async fn take_chunk_reports_an_empty_stream_as_exhausted_immediately() {
        let (buffer, exhausted) = take_chunk(&mut stream_of(&[]), 16).await.unwrap();
        assert!(buffer.is_empty());
        assert!(exhausted);
    }

    #[tokio::test]
    async fn take_chunk_propagates_a_stream_failure() {
        struct Failing;

        impl Stream for Failing {
            type Item = Result<Bytes, StorageError>;
            fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Ready(Some(Err(StorageError::Io("short read".to_owned()))))
            }
        }

        let error = take_chunk(&mut StorageStream::new(Failing), 16)
            .await
            .unwrap_err();
        assert!(matches!(error, StorageError::Io(_)));
    }

    #[test]
    fn the_part_size_satisfies_s3s_multipart_limits() {
        // Every one of these is decidable at compile time, so a bad constant fails the build
        // rather than the test run.
        const MINIMUM_PART_SIZE: usize = 5 * 1024 * 1024;
        /// The largest object this part size can carry, which the module docs quote as ~78 GiB.
        const LARGEST_OBJECT: u64 = SINGLE_PUT_THRESHOLD as u64
            + MULTIPART_PART_SIZE as u64 * (MAX_MULTIPART_PARTS as u64 - 1);

        // S3 requires every part but the last to be at least 5 MiB.
        const { assert!(MULTIPART_PART_SIZE >= MINIMUM_PART_SIZE) };
        // The first part is whatever the single-put probe already read, so the threshold has to be
        // a legal part size too.
        const { assert!(SINGLE_PUT_THRESHOLD >= MINIMUM_PART_SIZE) };
        // A body that reaches multipart is bigger than one request would have carried.
        const { assert!(SINGLE_PUT_THRESHOLD >= MULTIPART_PART_SIZE) };
        const { assert!(LARGEST_OBJECT > 78 * 1024 * 1024 * 1024) };
    }

    #[test]
    fn multipart_honours_every_option_but_the_checksum() {
        assert_eq!(SINGLE_PUT_OPTIONS, PutOption::ALL);
        for option in MULTIPART_PUT_OPTIONS {
            assert!(SINGLE_PUT_OPTIONS.contains(&option));
        }
        assert!(!MULTIPART_PUT_OPTIONS.contains(&PutOption::ContentMd5));
        assert_eq!(MULTIPART_PUT_OPTIONS.len(), SINGLE_PUT_OPTIONS.len() - 1);
    }

    #[test]
    fn storage_class_accepts_s3s_own_names_and_reports_a_typo() {
        assert_eq!(storage_class("STANDARD").unwrap().as_str(), "STANDARD");
        assert_eq!(storage_class("GLACIER").unwrap().as_str(), "GLACIER");
        assert_eq!(
            storage_class("INTELLIGENT_TIERING").unwrap().as_str(),
            "INTELLIGENT_TIERING"
        );

        let error = storage_class("Standard").unwrap_err();
        assert!(error.to_string().contains("STANDARD"), "{error}");
    }

    #[test]
    fn content_md5_is_base64_of_the_raw_digest() {
        // The MD5 of the empty string, which S3 documents as `1B2M2Y8AsgTpgAmY7PhCfg==`.
        let digest = [
            0xD4, 0x1D, 0x8C, 0xD9, 0x8F, 0x00, 0xB2, 0x04, 0xE9, 0x80, 0x09, 0x98, 0xEC, 0xF8,
            0x42, 0x7E,
        ];
        assert_eq!(content_md5_header(&digest), "1B2M2Y8AsgTpgAmY7PhCfg==");
    }

    #[tokio::test]
    async fn presign_get_signs_a_get_the_client_can_follow() {
        let presigned = storage()
            .presign_get("reports/q3.pdf", core::time::Duration::from_mins(15))
            .await
            .unwrap();

        assert_eq!(presigned.method, http_kit::Method::GET);
        assert!(presigned.url.contains("skyzen-tests"), "{}", presigned.url);
        assert!(
            presigned.url.contains("reports/q3.pdf"),
            "{}",
            presigned.url
        );
        assert!(
            presigned.url.contains("X-Amz-Signature="),
            "{}",
            presigned.url
        );
        assert!(
            presigned.url.contains("X-Amz-Expires=900"),
            "{}",
            presigned.url
        );
        assert!(
            presigned.url.contains("X-Amz-Credential=AKIDTEST"),
            "{}",
            presigned.url
        );
    }

    #[tokio::test]
    async fn presign_put_signs_a_put_and_carries_the_headers_the_client_must_send() {
        let presigned = storage()
            .presign_put(
                "uploads/avatar.png",
                core::time::Duration::from_mins(5),
                PutOptions::new()
                    .with_content_type("image/png")
                    .with_cache_control("public, max-age=31536000"),
            )
            .await
            .unwrap();

        assert_eq!(presigned.method, http_kit::Method::PUT);
        assert!(
            presigned.url.contains("X-Amz-Signature="),
            "{}",
            presigned.url
        );
        assert!(
            presigned.url.contains("X-Amz-Expires=300"),
            "{}",
            presigned.url
        );

        // Any header folded into the signature has to be repeated by the client verbatim.
        let signed: Vec<&str> = presigned
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        for (name, value) in &presigned.headers {
            assert!(
                !value.is_empty(),
                "{name} was signed with an empty value: {signed:?}"
            );
        }
    }

    #[tokio::test]
    async fn presigning_refuses_a_lifetime_sigv4_cannot_sign() {
        let error = storage()
            .presign_get("reports/q3.pdf", core::time::Duration::from_hours(24 * 8))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("one week"), "{error}");
    }
}
