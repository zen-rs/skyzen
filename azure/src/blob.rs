//! Azure Blob Storage implementation of [`ObjectStorage`].

mod rest;

use core::time::Duration;
use std::{collections::HashMap, sync::Arc};

use futures_util::StreamExt as _;
use http::{header::HeaderName, HeaderValue, Method};
use opendal::{services::Azblob, ErrorKind, Operator};
use reqsign_azure_storage::Credential;
use skyzen_services::storage::{
    ByteRange, ListOptions, ListResult, ObjectMetadata, ObjectStorage, PresignedRequest, PutOption,
    PutOptions, StorageError, StorageObject, StorageStream,
};

use self::rest::{BlobRest, READ_PERMISSION, WRITE_PERMISSION};

/// The environment variable [`AzureBlobConfig::from_env`] reads its connection string from.
const CONNECTION_STRING_ENV: &str = "AZURE_STORAGE_CONNECTION_STRING";

/// The endpoint suffix a public-cloud storage account lives under.
const DEFAULT_ENDPOINT_SUFFIX: &str = "core.windows.net";

/// The [`PutOptions`] fields a write through this backend records.
///
/// `Content-Encoding` and `Content-Disposition` are missing because `OpenDAL`'s azblob writer has
/// no header for either (its capability set reports neither), so asking for one would drop it
/// silently. `Content-MD5` is missing for the same reason. A caller that needs them uploads through
/// [`presign_put`](ObjectStorage::presign_put), whose request the client builds itself.
const HONOURED_PUT_OPTIONS: [PutOption; 2] = [PutOption::CacheControl, PutOption::StorageClass];

/// How much of a streamed upload is buffered into one block.
///
/// A block blob is assembled from at most 50,000 blocks, and `OpenDAL` sends one block per chunk it
/// is handed unless it is told otherwise — so a body arriving in 8 KB pieces would spend a request
/// on each and run out of blocks before 400 MB. Buffering 4 MiB per block is Azure's own
/// recommended block size and lifts that ceiling to 200 GB.
const UPLOAD_BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// How a storage account authenticates.
#[derive(Clone)]
pub enum AzureBlobAuth {
    /// One of the account's two access keys, base64 as the portal shows it.
    ///
    /// The only credential that can mint a shared access signature, so
    /// [`presign_get`](ObjectStorage::presign_get) and [`presign_put`](ObjectStorage::presign_put)
    /// need it.
    AccountKey(String),

    /// A shared access signature, as the query string it is issued as (`sv=…&sig=…`).
    ///
    /// Every data operation works, but a signature cannot mint a narrower signature, so presigning
    /// reports [`StorageError::Unsupported`].
    SasToken(String),
}

impl core::fmt::Debug for AzureBlobAuth {
    /// Never renders the secret: a config is the kind of thing that ends up in a log line.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AccountKey(_) => f.write_str("AccountKey(<redacted>)"),
            Self::SasToken(_) => f.write_str("SasToken(<redacted>)"),
        }
    }
}

/// Everything needed to reach one blob container.
///
/// The credentials belong to this config rather than to a third-party builder, so the same account
/// key that moves the bytes can also sign a listing's continuation and mint a presigned URL.
#[derive(Clone, Debug)]
pub struct AzureBlobConfig {
    /// The storage account name.
    pub account: String,
    /// The container inside it.
    pub container: String,
    /// How to authenticate.
    pub auth: AzureBlobAuth,
    /// The account's blob endpoint, when it is not `https://{account}.blob.core.windows.net` —
    /// a sovereign cloud, or Azurite on `http://127.0.0.1:10000/devstoreaccount1`.
    pub endpoint: Option<String>,
}

impl AzureBlobConfig {
    /// Reach `container` in `account` with `auth`.
    #[must_use]
    pub fn new(
        account: impl Into<String>,
        container: impl Into<String>,
        auth: AzureBlobAuth,
    ) -> Self {
        Self {
            account: account.into(),
            container: container.into(),
            auth,
            endpoint: None,
        }
    }

    /// Reach the account at `endpoint` rather than at the public-cloud host.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Parse a storage account connection string.
    ///
    /// Both forms the portal hands out are read: an account key
    /// (`DefaultEndpointsProtocol=…;AccountName=…;AccountKey=…;EndpointSuffix=…`) and a shared
    /// access signature (`BlobEndpoint=…;SharedAccessSignature=…`). An explicit `BlobEndpoint`
    /// wins over the one built from the account name and suffix, which is what makes an Azurite
    /// connection string work.
    ///
    /// # Errors
    ///
    /// [`StorageError::Backend`] when the string is not name=value pairs, names no account, or
    /// carries no credential.
    pub fn from_connection_string(
        connection_string: &str,
        container: impl Into<String>,
    ) -> Result<Self, StorageError> {
        let parsed = ConnectionString::parse(connection_string)?;

        Ok(Self {
            account: parsed.account,
            container: container.into(),
            auth: parsed.auth,
            endpoint: Some(parsed.endpoint),
        })
    }

    /// Read the connection string in `AZURE_STORAGE_CONNECTION_STRING`.
    ///
    /// # Errors
    ///
    /// [`StorageError::Backend`] when the variable is unset or its value is not a usable
    /// connection string — see [`from_connection_string`](Self::from_connection_string).
    pub fn from_env(container: impl Into<String>) -> Result<Self, StorageError> {
        let connection_string = std::env::var(CONNECTION_STRING_ENV).map_err(|error| {
            StorageError::backend_with(
                format!("{CONNECTION_STRING_ENV} is not set to a storage connection string"),
                error,
            )
        })?;

        Self::from_connection_string(&connection_string, container)
    }

    /// The blob endpoint of the account, explicit or derived from its name.
    fn endpoint(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| format!("https://{}.blob.{DEFAULT_ENDPOINT_SUFFIX}", self.account))
    }

    /// The credential in the form the request signer takes.
    fn credential(&self) -> Credential {
        match &self.auth {
            AzureBlobAuth::AccountKey(key) => Credential::SharedKey {
                account_name: self.account.clone(),
                account_key: key.clone(),
            },
            AzureBlobAuth::SasToken(token) => Credential::SasToken {
                token: token.clone(),
            },
        }
    }
}

/// A parsed storage account connection string.
#[derive(Debug, Clone)]
struct ConnectionString {
    /// The account name.
    account: String,
    /// The blob endpoint, explicit or built from the account name and endpoint suffix.
    endpoint: String,
    /// The credential it carries.
    auth: AzureBlobAuth,
}

impl ConnectionString {
    /// Parse `Name=value;Name=value;…`.
    fn parse(connection_string: &str) -> Result<Self, StorageError> {
        let mut protocol = None;
        let mut account = None;
        let mut key = None;
        let mut suffix = None;
        let mut blob_endpoint = None;
        let mut signature = None;

        for part in connection_string.split(';').filter(|part| !part.is_empty()) {
            // A value can itself contain `=` — a base64 account key ends in one, and a signature is
            // a whole query string — so only the first separator splits.
            let (name, value) = part.split_once('=').ok_or_else(|| {
                StorageError::backend(format!(
                    "the storage connection string holds {part:?}, which is not a name=value pair"
                ))
            })?;

            let value = value.trim().to_owned();
            match name.trim() {
                name if name.eq_ignore_ascii_case("DefaultEndpointsProtocol") => {
                    protocol = Some(value);
                }
                name if name.eq_ignore_ascii_case("AccountName") => account = Some(value),
                name if name.eq_ignore_ascii_case("AccountKey") => key = Some(value),
                name if name.eq_ignore_ascii_case("EndpointSuffix") => suffix = Some(value),
                name if name.eq_ignore_ascii_case("BlobEndpoint") => blob_endpoint = Some(value),
                name if name.eq_ignore_ascii_case("SharedAccessSignature") => {
                    signature = Some(value);
                }
                _ => {}
            }
        }

        let account = account.ok_or_else(|| {
            StorageError::backend("the storage connection string has no AccountName")
        })?;

        let auth = match (key, signature) {
            (Some(key), _) => AzureBlobAuth::AccountKey(key),
            (None, Some(signature)) => AzureBlobAuth::SasToken(signature),
            (None, None) => {
                return Err(StorageError::backend(
                    "the storage connection string carries neither an AccountKey nor a \
                     SharedAccessSignature",
                ))
            }
        };

        let endpoint = blob_endpoint.unwrap_or_else(|| {
            format!(
                "{}://{account}.blob.{}",
                protocol.as_deref().unwrap_or("https"),
                suffix.as_deref().unwrap_or(DEFAULT_ENDPOINT_SUFFIX)
            )
        });

        Ok(Self {
            account,
            endpoint,
            auth,
        })
    }
}

/// An Azure Blob Storage-backed object store.
///
/// Bytes move through [Apache `OpenDAL`](https://opendal.apache.org), while listing, tiering and
/// presigning are Azure REST calls this crate issues itself — which is what lets a paginated
/// listing resume from the service's own marker rather than re-reading everything it has already
/// returned.
///
/// # Construction
///
/// [`AzureBlob::from_env`] reads `AZURE_STORAGE_CONNECTION_STRING`;
/// [`AzureBlob::from_connection_string`] takes one directly; [`AzureBlob::new`] takes an
/// [`AzureBlobConfig`] assembled by hand.
///
/// # Presigning
///
/// Minting a shared access signature needs an account key. Under
/// [`AzureBlobAuth::SasToken`] the presign methods report [`StorageError::Unsupported`] rather than
/// handing the caller the account's own signature, which would carry the wrong scope and the wrong
/// expiry.
///
/// Cloning is cheap — the `OpenDAL` operator and the REST endpoint are both behind `Arc`.
#[derive(Clone, Debug)]
pub struct AzureBlob {
    /// Moves the bytes.
    operator: Operator,
    /// Issues the REST calls `OpenDAL` does not cover.
    rest: Arc<BlobRest>,
}

impl AzureBlob {
    /// Reach the container `config` names.
    ///
    /// # Errors
    ///
    /// [`StorageError::Backend`] when the endpoint is not a URL or the credentials are not enough
    /// to reach the container.
    pub fn new(config: AzureBlobConfig) -> Result<Self, StorageError> {
        let endpoint = config.endpoint();
        let credential = config.credential();
        let AzureBlobConfig {
            account,
            container,
            auth,
            ..
        } = config;

        let mut builder = Azblob::default()
            .container(&container)
            .endpoint(&endpoint)
            .account_name(&account);

        builder = match &auth {
            AzureBlobAuth::AccountKey(key) => builder.account_key(key),
            AzureBlobAuth::SasToken(token) => builder.sas_token(token),
        };

        let operator = Operator::new(builder).map_err(storage_error)?;
        let rest = BlobRest::new(&endpoint, &container, credential)?;

        Ok(Self {
            operator,
            rest: Arc::new(rest),
        })
    }

    /// Reach a container using the connection string in `AZURE_STORAGE_CONNECTION_STRING`.
    ///
    /// # Errors
    ///
    /// [`StorageError::Backend`] when the variable is unset or unusable — see
    /// [`AzureBlobConfig::from_env`].
    pub fn from_env(container: impl Into<String>) -> Result<Self, StorageError> {
        Self::new(AzureBlobConfig::from_env(container)?)
    }

    /// Reach a container using a storage account connection string.
    ///
    /// # Errors
    ///
    /// [`StorageError::Backend`] when the string is unusable — see
    /// [`AzureBlobConfig::from_connection_string`].
    pub fn from_connection_string(
        connection_string: &str,
        container: impl Into<String>,
    ) -> Result<Self, StorageError> {
        Self::new(AzureBlobConfig::from_connection_string(
            connection_string,
            container,
        )?)
    }

    /// Move a freshly written blob to the tier the write asked for.
    ///
    /// A tier is a separate REST call because `OpenDAL`'s writer has no header for it, so a write
    /// that asked for one is a write followed by a `Set Blob Tier`: the blob exists at the account's
    /// default tier in between.
    async fn apply_storage_class(
        &self,
        key: &str,
        storage_class: Option<&String>,
    ) -> Result<(), StorageError> {
        match storage_class {
            None => Ok(()),
            Some(tier) => self.rest.set_blob_tier(key, tier).await,
        }
    }
}

/// Record the [`PutOptions`] this backend can carry on one of `OpenDAL`'s write builders.
///
/// A buffered write and a streamed one take different builder types with the same setters, so the
/// options are applied by macro rather than by duplicating the six lines twice.
macro_rules! with_put_options {
    ($request:expr, $options:expr) => {{
        let mut request = $request;
        if let Some(content_type) = &$options.content_type {
            request = request.content_type(content_type);
        }
        if let Some(cache_control) = &$options.cache_control {
            request = request.cache_control(cache_control);
        }
        if !$options.metadata.is_empty() {
            request = request.user_metadata($options.metadata.clone());
        }
        request
    }};
}

impl ObjectStorage for AzureBlob {
    async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        let body = match self.operator.read(key).await {
            Ok(body) => body.to_vec(),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage_error(error)),
        };

        // Fetch real content type / timestamps so `get` and `head` agree.
        let metadata = match self.operator.stat(key).await {
            Ok(stat) => {
                let mut metadata = stat_metadata(key, &stat);
                metadata.size = size_of(body.len())?;
                metadata
            }
            // The blob vanished between read and stat; report the read we made.
            Err(error) if error.kind() == ErrorKind::NotFound => ObjectMetadata {
                key: key.to_owned(),
                size: size_of(body.len())?,
                content_type: None,
                last_modified: None,
                metadata: HashMap::new(),
                etag: None,
                version: None,
            },
            Err(error) => return Err(storage_error(error)),
        };

        Ok(Some(StorageObject { body, metadata }))
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        self.operator
            .write(key, body)
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    /// Store an object, recording the options Azure Blob carries on a write.
    ///
    /// A storage tier is applied by a `Set Blob Tier` call right after the write, since the write
    /// itself cannot carry one; the options `OpenDAL`'s writer has no header for are refused rather
    /// than dropped.
    async fn put_with(
        &self,
        key: &str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        options.reject_unsupported(&HONOURED_PUT_OPTIONS)?;

        with_put_options!(self.operator.write_with(key, body), options)
            .await
            .map_err(storage_error)?;

        self.apply_storage_class(key, options.storage_class.as_ref())
            .await
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match self.operator.delete(key).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(storage_error(error)),
        }
    }

    /// List one page of blobs, resuming from Azure's own `marker`.
    ///
    /// The cursor is the service's continuation marker, so a page costs one request no matter how
    /// far into the container it is. It belongs to the listing that produced it: resume with the
    /// same [`ListOptions::prefix`].
    ///
    /// [`ObjectMetadata::metadata`] is always empty here and
    /// [`ObjectMetadata::version`] always `None`: `List Blobs` reports a name, a size, a
    /// modification time, a content type and an `ETag`, and asking for more per key would turn one
    /// request into hundreds. A caller that needs the custom metadata of a specific object asks
    /// [`head`](ObjectStorage::head) for it.
    async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        // A zero limit yields an empty page, consistent with other backends; the incoming cursor is
        // echoed back so no listing progress is lost.
        if options.limit == Some(0) {
            return Ok(ListResult {
                objects: Vec::new(),
                cursor: options.cursor,
            });
        }

        let page = self
            .rest
            .list_blobs(
                options.prefix.as_deref(),
                options.cursor.as_deref(),
                options.limit,
            )
            .await?;

        let cursor = page.cursor();
        let objects = page
            .blobs
            .blob
            .into_iter()
            .map(|blob| ObjectMetadata {
                key: blob.name,
                size: blob.properties.content_length,
                content_type: Some(blob.properties.content_type.clone())
                    .filter(|content_type| !content_type.is_empty()),
                last_modified: blob.properties.last_modified_seconds(),
                metadata: HashMap::new(),
                etag: blob.properties.quoted_etag(),
                version: None,
            })
            .collect();

        Ok(ListResult { objects, cursor })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        match self.operator.stat(key).await {
            Ok(metadata) => Ok(Some(stat_metadata(key, &metadata))),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_error(error)),
        }
    }

    /// Stream a blob's body without buffering it.
    ///
    /// The blob is stat'd first so an absent one reports `None` here rather than as an error on the
    /// first chunk a caller pulls.
    async fn get_stream(&self, key: &str) -> Result<Option<StorageStream>, StorageError> {
        if self.head(key).await?.is_none() {
            return Ok(None);
        }

        let reader = self
            .operator
            .reader_with(key)
            .await
            .map_err(storage_error)?;

        let stream = reader
            .into_bytes_stream(..)
            .await
            .map_err(storage_error)?
            .map(|chunk| chunk.map_err(|error| StorageError::Io(error.to_string())));

        Ok(Some(StorageStream::new(stream)))
    }

    /// Upload from a stream, one [`UPLOAD_BLOCK_SIZE`] block at a time.
    ///
    /// A `content_length` that disagrees with what the stream actually yielded fails the upload
    /// rather than storing a truncated or over-long object — the blob is deleted first, so a
    /// mismatch does not leave a half-written object behind.
    async fn put_stream(
        &self,
        key: &str,
        mut stream: StorageStream,
        content_length: Option<u64>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        options.reject_unsupported(&HONOURED_PUT_OPTIONS)?;

        let mut writer = with_put_options!(
            self.operator.writer_with(key).chunk(UPLOAD_BLOCK_SIZE),
            options
        )
        .await
        .map_err(storage_error)?;

        // Every way out of the upload but a successful `close` aborts the writer, so the blocks it
        // has already staged are released rather than left for Azure to garbage-collect a week
        // later — a stream that fails half way through is the ordinary case here, not a rare one.
        let mut written: u64 = 0;
        loop {
            let chunk = match stream.next().await {
                None => break,
                Some(Ok(chunk)) => chunk,
                Some(Err(error)) => return abort_upload(&mut writer, error).await,
            };

            written = match written.checked_add(size_of(chunk.len())?) {
                Some(written) => written,
                None => {
                    return abort_upload(
                        &mut writer,
                        StorageError::backend("the streamed upload overflowed u64"),
                    )
                    .await
                }
            };

            if let Err(error) = writer.write(chunk).await {
                return abort_upload(&mut writer, storage_error(error)).await;
            }
        }

        if let Some(declared) = content_length {
            if declared != written {
                return abort_upload(
                    &mut writer,
                    StorageError::backend(format!(
                        "streamed upload of {key:?} declared {declared} bytes but the stream \
                         yielded {written}"
                    )),
                )
                .await;
            }
        }

        writer.close().await.map_err(storage_error)?;

        self.apply_storage_class(key, options.storage_class.as_ref())
            .await
    }

    /// Read part of a blob, letting Azure serve only the requested bytes.
    ///
    /// The blob is stat'd first, which is what resolves a suffix range and what lets the returned
    /// metadata keep reporting the size of the whole object while the body holds only the slice.
    async fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> Result<Option<StorageObject>, StorageError> {
        let Some(metadata) = self.head(key).await? else {
            return Ok(None);
        };

        let resolved = range.resolve(metadata.size).ok_or_else(|| {
            StorageError::backend(format!(
                "byte range {range:?} selects no bytes of {key:?}, which is {} bytes",
                metadata.size
            ))
        })?;

        let body = match self.operator.read_with(key).range(resolved).await {
            Ok(body) => body.to_vec(),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage_error(error)),
        };

        Ok(Some(StorageObject { body, metadata }))
    }

    /// Mint a URL a browser can follow to download the blob directly.
    ///
    /// Signing is local: no request reaches Azure, and the URL is valid whether or not the blob
    /// exists yet.
    async fn presign_get(
        &self,
        key: &str,
        expires_in: Duration,
    ) -> Result<PresignedRequest, StorageError> {
        self.presigned(key, &Method::GET, READ_PERMISSION, expires_in, Vec::new())
            .await
    }

    /// Mint a URL a browser can upload to directly, keeping a large upload off the server.
    ///
    /// An Azure service signature covers the URL, not the request's headers, so the [`PutOptions`]
    /// asked for here come back as [`PresignedRequest::headers`] for the client to send: they shape
    /// the stored blob, and a client that drops them uploads a blob without them. `x-ms-blob-type`
    /// is always there — Azure rejects a block blob upload that omits it.
    async fn presign_put(
        &self,
        key: &str,
        expires_in: Duration,
        options: PutOptions,
    ) -> Result<PresignedRequest, StorageError> {
        self.presigned(
            key,
            &Method::PUT,
            WRITE_PERMISSION,
            expires_in,
            upload_headers(&options)?,
        )
        .await
    }
}

impl AzureBlob {
    /// Mint one presigned request, refusing to when the credential cannot sign a signature.
    async fn presigned(
        &self,
        key: &str,
        method: &Method,
        permissions: &str,
        expires_in: Duration,
        headers: Vec<(HeaderName, HeaderValue)>,
    ) -> Result<PresignedRequest, StorageError> {
        if !self.rest.can_presign() {
            return Err(StorageError::Unsupported(
                "a shared access signature cannot mint another one; configure this store with \
                 AzureBlobAuth::AccountKey to presign",
            ));
        }

        Ok(PresignedRequest {
            url: self
                .rest
                .presign(key, method, permissions, expires_in)
                .await?,
            method: method.clone(),
            headers,
        })
    }
}

/// The headers a client must send with a presigned upload.
///
/// Every [`PutOptions`] field maps onto one, which is why presigning honours the options a buffered
/// write cannot: the client builds the request, so it can carry headers `OpenDAL`'s writer does not.
fn upload_headers(options: &PutOptions) -> Result<Vec<(HeaderName, HeaderValue)>, StorageError> {
    let mut headers = vec![(
        HeaderName::from_static("x-ms-blob-type"),
        HeaderValue::from_static("BlockBlob"),
    )];

    let mut push = |name: HeaderName, value: &str| -> Result<(), StorageError> {
        headers.push((
            name.clone(),
            HeaderValue::from_str(value).map_err(|error| {
                StorageError::backend_with(
                    format!("{value:?} cannot be sent as the {name} of an upload"),
                    error,
                )
            })?,
        ));
        Ok(())
    };

    if let Some(content_type) = &options.content_type {
        push(
            HeaderName::from_static("x-ms-blob-content-type"),
            content_type,
        )?;
    }
    if let Some(cache_control) = &options.cache_control {
        push(
            HeaderName::from_static("x-ms-blob-cache-control"),
            cache_control,
        )?;
    }
    if let Some(content_encoding) = &options.content_encoding {
        push(
            HeaderName::from_static("x-ms-blob-content-encoding"),
            content_encoding,
        )?;
    }
    if let Some(content_disposition) = &options.content_disposition {
        push(
            HeaderName::from_static("x-ms-blob-content-disposition"),
            content_disposition,
        )?;
    }
    if let Some(storage_class) = &options.storage_class {
        push(HeaderName::from_static("x-ms-access-tier"), storage_class)?;
    }
    if let Some(digest) = &options.content_md5 {
        use base64::Engine as _;
        push(
            HeaderName::from_static("content-md5"),
            &base64::engine::general_purpose::STANDARD.encode(digest),
        )?;
    }
    for (name, value) in &options.metadata {
        let name = HeaderName::try_from(format!("x-ms-meta-{name}")).map_err(|error| {
            StorageError::backend_with(
                format!("{name:?} cannot be sent as custom metadata on an upload"),
                error,
            )
        })?;
        push(name, value)?;
    }

    Ok(headers)
}

/// Give up on a streamed upload, releasing the blocks it has already staged.
///
/// The abort's own failure is reported as the source of `error` rather than replacing it: what the
/// caller needs to know is why the upload stopped, and the leftover blocks expire on their own.
async fn abort_upload(
    writer: &mut opendal::Writer,
    error: StorageError,
) -> Result<(), StorageError> {
    if let Err(abort) = writer.abort().await {
        return Err(StorageError::backend_with(
            format!("{error} (the upload's staged blocks could not be released: {abort})"),
            error,
        ));
    }

    Err(error)
}

/// Read `OpenDAL`'s metadata into the portable shape.
fn stat_metadata(key: &str, metadata: &opendal::Metadata) -> ObjectMetadata {
    ObjectMetadata {
        key: key.to_owned(),
        size: metadata.content_length(),
        content_type: metadata.content_type().map(ToOwned::to_owned),
        last_modified: metadata
            .last_modified()
            .and_then(|timestamp| u64::try_from(timestamp.into_inner().as_second()).ok()),
        metadata: metadata.user_metadata().cloned().unwrap_or_default(),
        etag: metadata.etag().map(ToOwned::to_owned),
        version: metadata.version().map(ToOwned::to_owned),
    }
}

/// A byte count as the `u64` the portable metadata reports.
fn size_of(length: usize) -> Result<u64, StorageError> {
    u64::try_from(length)
        .map_err(|error| StorageError::backend_with("Azure blob size overflow", error))
}

/// Map an `OpenDAL` error onto the portable taxonomy, keeping its source chain.
fn storage_error(error: opendal::Error) -> StorageError {
    match error.kind() {
        ErrorKind::RateLimited => StorageError::Throttled { retry_after: None },
        ErrorKind::PermissionDenied => StorageError::Unauthorized,
        ErrorKind::ConditionNotMatch | ErrorKind::AlreadyExists => StorageError::Conflict,
        _ => StorageError::backend_with(error.to_string(), error),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        upload_headers, AzureBlob, AzureBlobAuth, AzureBlobConfig, ConnectionString,
        HONOURED_PUT_OPTIONS,
    };
    use skyzen_services::storage::{ObjectStorage, PutOption, PutOptions, StorageError};

    const CONNECTION_STRING: &str = "DefaultEndpointsProtocol=https;AccountName=skyzentest;\
         AccountKey=c2t5emVuLXRlc3Qta2V5;EndpointSuffix=core.windows.net";

    const AZURITE: &str = "DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;\
         AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;\
         BlobEndpoint=http://127.0.0.1:10000/devstoreaccount1";

    fn store() -> AzureBlob {
        AzureBlob::from_connection_string(CONNECTION_STRING, "uploads")
            .expect("the connection string should build a store")
    }

    #[test]
    fn a_connection_string_yields_an_account_key_and_the_public_cloud_endpoint() {
        let parsed = ConnectionString::parse(CONNECTION_STRING).expect("should parse");
        assert_eq!(parsed.account, "skyzentest");
        assert_eq!(parsed.endpoint, "https://skyzentest.blob.core.windows.net");
        assert!(
            matches!(parsed.auth, AzureBlobAuth::AccountKey(key) if key == "c2t5emVuLXRlc3Qta2V5")
        );
    }

    #[test]
    fn an_explicit_blob_endpoint_wins_so_azurite_works() {
        let parsed = ConnectionString::parse(AZURITE).expect("should parse");
        assert_eq!(parsed.account, "devstoreaccount1");
        assert_eq!(parsed.endpoint, "http://127.0.0.1:10000/devstoreaccount1");
    }

    #[test]
    fn a_signature_connection_string_authenticates_without_a_key() {
        let parsed = ConnectionString::parse(
            "BlobEndpoint=https://skyzentest.blob.core.windows.net;AccountName=skyzentest;\
             SharedAccessSignature=sv=2020-12-06&ss=b&sig=abc=",
        )
        .expect("should parse");
        // The signature keeps every character of its own query string, `=` padding included.
        assert!(
            matches!(parsed.auth, AzureBlobAuth::SasToken(token) if token == "sv=2020-12-06&ss=b&sig=abc=")
        );
    }

    #[test]
    fn a_custom_endpoint_suffix_reaches_a_sovereign_cloud() {
        let parsed = ConnectionString::parse(
            "DefaultEndpointsProtocol=https;AccountName=skyzentest;AccountKey=a2V5;\
             EndpointSuffix=core.chinacloudapi.cn",
        )
        .expect("should parse");
        assert_eq!(
            parsed.endpoint,
            "https://skyzentest.blob.core.chinacloudapi.cn"
        );
    }

    #[test]
    fn a_malformed_connection_string_is_refused() {
        assert!(ConnectionString::parse("").is_err());
        assert!(ConnectionString::parse("AccountName").is_err());
        // No credential at all.
        assert!(ConnectionString::parse("AccountName=skyzentest").is_err());
        // No account.
        assert!(ConnectionString::parse("AccountKey=a2V5").is_err());
    }

    #[test]
    fn a_config_without_an_endpoint_addresses_the_public_cloud() {
        let config = AzureBlobConfig::new(
            "skyzentest",
            "uploads",
            AzureBlobAuth::AccountKey("a2V5".to_owned()),
        );
        assert_eq!(
            config.endpoint(),
            "https://skyzentest.blob.core.windows.net"
        );
        assert_eq!(
            config
                .with_endpoint("http://127.0.0.1:10000/devstoreaccount1")
                .endpoint(),
            "http://127.0.0.1:10000/devstoreaccount1"
        );
    }

    #[test]
    fn a_config_never_renders_its_credentials() {
        let config = AzureBlobConfig::new(
            "skyzentest",
            "uploads",
            AzureBlobAuth::AccountKey("super-secret".to_owned()),
        );
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[tokio::test]
    async fn a_put_option_this_backend_would_drop_is_refused() {
        let store = store();

        for options in [
            PutOptions::new().with_content_encoding("gzip"),
            PutOptions::new().with_content_disposition("attachment"),
            PutOptions::new().with_content_md5([0_u8; 16]),
        ] {
            let error = store
                .put_with("k", Vec::new(), options)
                .await
                .expect_err("an option OpenDAL cannot carry should be refused");
            assert!(matches!(error, StorageError::Unsupported(_)));
        }
    }

    #[test]
    fn the_honoured_options_are_the_ones_the_write_really_records() {
        assert!(HONOURED_PUT_OPTIONS.contains(&PutOption::CacheControl));
        assert!(HONOURED_PUT_OPTIONS.contains(&PutOption::StorageClass));
        assert!(!HONOURED_PUT_OPTIONS.contains(&PutOption::ContentEncoding));
    }

    #[test]
    fn a_presigned_upload_carries_every_option_as_a_header() {
        let options = PutOptions::new()
            .with_content_type("image/png")
            .with_cache_control("max-age=60")
            .with_content_encoding("gzip")
            .with_content_disposition("attachment")
            .with_storage_class("Cool")
            .with_content_md5([0_u8; 16])
            .with_metadata("owner", "skyzen");

        let headers = upload_headers(&options).expect("every option should map to a header");
        let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();

        // Azure rejects a block blob upload that does not declare its type.
        assert!(names.contains(&"x-ms-blob-type"));
        assert!(names.contains(&"x-ms-blob-content-type"));
        assert!(names.contains(&"x-ms-blob-cache-control"));
        assert!(names.contains(&"x-ms-blob-content-encoding"));
        assert!(names.contains(&"x-ms-blob-content-disposition"));
        assert!(names.contains(&"x-ms-access-tier"));
        assert!(names.contains(&"content-md5"));
        assert!(names.contains(&"x-ms-meta-owner"));
    }

    #[tokio::test]
    async fn presigning_under_a_shared_signature_is_refused_rather_than_leaking_the_account_one() {
        let store = AzureBlob::new(AzureBlobConfig::new(
            "skyzentest",
            "uploads",
            AzureBlobAuth::SasToken("sv=2020-12-06&sig=signature".to_owned()),
        ))
        .expect("a signature should build a store");

        let error = store
            .presign_get("photos/one.png", core::time::Duration::from_mins(15))
            .await
            .expect_err("a signature cannot mint another");
        assert!(matches!(error, StorageError::Unsupported(_)));
    }

    #[tokio::test]
    async fn a_presigned_download_is_a_url_the_client_can_follow() {
        let request = store()
            .presign_get("photos/one.png", core::time::Duration::from_mins(15))
            .await
            .expect("an account key should sign");

        assert_eq!(request.method, http::Method::GET);
        assert!(request.headers.is_empty());
        assert!(request.url.contains("sig="), "{}", request.url);
    }
}
