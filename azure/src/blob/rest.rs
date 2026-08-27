//! The Azure Blob REST calls `OpenDAL` does not expose.
//!
//! `OpenDAL` moves the bytes, but three things it cannot do are what a storage backend needs:
//! listing from a server-side `marker` (its lister owns its own paging and cannot resume from an
//! outside cursor), minting a shared access signature, and setting a blob's access tier. All three
//! are ordinary Azure REST calls, so this module issues them on the same `reqwest` client and the
//! same `reqsign` signer `OpenDAL`'s own azblob service is built on — the request is signed exactly
//! the way the data-plane requests beside it are.

use core::time::Duration;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, StatusCode, Uri};
use reqsign_azure_storage::{Credential, RequestSigner};
use reqsign_core::{Context, SignRequest as _};
use serde::Deserialize;
use skyzen_services::storage::StorageError;
use url::Url;

use crate::status::{classify, retry_after, AzureStatus};

/// The Azure Storage REST version this client speaks.
///
/// The version Azurite V3 and the Azure portal use, which is what `OpenDAL`'s azblob service sends
/// too, so a listing and the data-plane calls beside it are answered by the same service version.
const SERVICE_VERSION: &str = "2022-11-02";

/// The header naming the service version of a request.
const VERSION_HEADER: &str = "x-ms-version";

/// The header carrying the access tier of a `Set Blob Tier` request.
const ACCESS_TIER_HEADER: &str = "x-ms-access-tier";

/// The permissions a presigned download needs: read.
pub(super) const READ_PERMISSION: &str = "r";

/// The permissions a presigned upload needs: create and write.
pub(super) const WRITE_PERMISSION: &str = "cw";

/// The Azure Blob REST endpoint of one container.
pub(super) struct BlobRest {
    /// The HTTP client the requests go out on.
    client: reqwest::Client,
    /// The signer, which turns a request into a signed one or a URL into a presigned one.
    signer: RequestSigner,
    /// `reqsign`'s ambient context. Shared-key signing needs nothing from it.
    context: Context,
    /// The account credential.
    credential: Credential,
    /// The container's URL, `https://{account}.blob.core.windows.net/{container}`.
    container_url: Url,
}

impl core::fmt::Debug for BlobRest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BlobRest")
            .field("container_url", &self.container_url.as_str())
            .finish_non_exhaustive()
    }
}

impl BlobRest {
    /// Bind to the container at `endpoint`, authenticating with `credential`.
    pub(super) fn new(
        endpoint: &str,
        container: &str,
        credential: Credential,
    ) -> Result<Self, StorageError> {
        let mut container_url = Url::parse(endpoint).map_err(|error| {
            StorageError::backend_with(
                format!("{endpoint:?} is not an Azure Blob endpoint URL"),
                error,
            )
        })?;

        container_url
            .path_segments_mut()
            .map_err(|()| {
                StorageError::backend(format!("{endpoint:?} cannot carry a container name"))
            })?
            .pop_if_empty()
            .push(container);

        Ok(Self {
            client: reqwest::Client::new(),
            signer: RequestSigner::new(),
            context: Context::new(),
            credential,
            container_url,
        })
    }

    /// Whether this endpoint can mint a shared access signature of its own.
    ///
    /// Only an account key can sign one. A client that was itself handed a signature can pass that
    /// signature on, but not narrow it to one blob or one expiry, which is the whole point of
    /// presigning.
    pub(super) const fn can_presign(&self) -> bool {
        matches!(self.credential, Credential::SharedKey { .. })
    }

    /// The URL of one blob in this container.
    fn blob_url(&self, key: &str) -> Result<Url, StorageError> {
        let mut url = self.container_url.clone();
        url.path_segments_mut()
            .map_err(|()| StorageError::backend("the container URL cannot carry a blob name"))?
            .pop_if_empty()
            // A blob name is a path: its slashes separate segments, and everything else in each
            // segment is percent-encoded by the URL parser rather than by hand.
            .extend(key.split('/'));
        Ok(url)
    }

    /// List one page of blobs, resuming from the service's own `marker`.
    pub(super) async fn list_blobs(
        &self,
        prefix: Option<&str>,
        marker: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ListBlobsOutput, StorageError> {
        let mut url = self.container_url.clone();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("restype", "container");
            query.append_pair("comp", "list");
            if let Some(prefix) = prefix {
                query.append_pair("prefix", prefix);
            }
            if let Some(marker) = marker {
                query.append_pair("marker", marker);
            }
            if let Some(limit) = limit {
                query.append_pair("maxresults", &limit.to_string());
            }
        }

        let body = self.send(Method::GET, url, Vec::new(), &[]).await?;

        quick_xml::de::from_reader(body.as_ref()).map_err(|error| {
            StorageError::backend_with(
                "Azure answered a blob listing with a body this client could not parse",
                error,
            )
        })
    }

    /// Move a blob to another access tier.
    ///
    /// `OpenDAL`'s writer has no header for it, so a tier asked for at write time is applied right
    /// after the write.
    pub(super) async fn set_blob_tier(&self, key: &str, tier: &str) -> Result<(), StorageError> {
        let mut url = self.blob_url(key)?;
        url.query_pairs_mut().append_pair("comp", "tier");

        let tier = HeaderValue::from_str(tier).map_err(|error| {
            StorageError::backend_with(
                format!("{tier:?} is not a storage tier name Azure could carry in a header"),
                error,
            )
        })?;

        self.send(
            Method::PUT,
            url,
            Vec::new(),
            &[(HeaderName::from_static(ACCESS_TIER_HEADER), tier)],
        )
        .await?;
        Ok(())
    }

    /// Mint a URL carrying a service shared access signature for one blob.
    pub(super) async fn presign(
        &self,
        key: &str,
        method: &Method,
        permissions: &str,
        expires_in: Duration,
    ) -> Result<String, StorageError> {
        let mut parts = http::Request::builder()
            .method(method)
            .uri(uri(&self.blob_url(key)?)?)
            .body(())
            .map_err(|error| {
                StorageError::backend_with("failed to build a presigned request", error)
            })?
            .into_parts()
            .0;

        RequestSigner::new()
            .with_service_sas_permissions(permissions)
            .sign_request(
                &self.context,
                &mut parts,
                Some(&self.credential),
                Some(expires_in),
            )
            .await
            .map_err(|error| {
                StorageError::backend_with("failed to sign a shared access signature", error)
            })?;

        Ok(parts.uri.to_string())
    }

    /// Sign and issue one request, returning its body.
    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Vec<u8>,
        headers: &[(HeaderName, HeaderValue)],
    ) -> Result<Bytes, StorageError> {
        let mut request = http::Request::builder()
            .method(&method)
            .uri(uri(&url)?)
            // Every Azure Storage request names the service version it expects to be answered by;
            // `reqsign` signs whatever `x-ms-` headers the request already carries.
            .header(VERSION_HEADER, SERVICE_VERSION)
            .body(body)
            .map_err(|error| {
                StorageError::backend_with(format!("failed to build a {method} request"), error)
            })?;

        for (name, value) in headers {
            request.headers_mut().insert(name.clone(), value.clone());
        }

        let (mut parts, body) = request.into_parts();
        self.signer
            .sign_request(&self.context, &mut parts, Some(&self.credential), None)
            .await
            .map_err(|error| {
                StorageError::backend_with("failed to sign an Azure Blob request", error)
            })?;

        let request = reqwest::Request::try_from(http::Request::from_parts(parts, body))
            .map_err(|error| StorageError::backend_with("failed to build a request", error))?;

        let response = self.client.execute(request).await.map_err(|error| {
            StorageError::backend_with(format!("the {method} request to Azure failed"), error)
        })?;

        let status = response.status();
        let retry_after_header = response
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(status_error(
                status,
                &method,
                retry_after_header.as_deref(),
                &body,
            ));
        }

        response.bytes().await.map_err(|error| {
            StorageError::backend_with("failed to read Azure's response body", error)
        })
    }
}

/// The URI form of a URL, for the `http` request builder.
fn uri(url: &Url) -> Result<Uri, StorageError> {
    url.as_str()
        .parse()
        .map_err(|error| StorageError::backend_with(format!("{url} is not a request URI"), error))
}

/// Map a failing Azure status onto the portable taxonomy, keeping the service's own message.
///
/// Azure answers a failed request with an XML body naming the error code, which is the difference
/// between "this container does not exist" and "this signature has expired".
fn status_error(
    status: StatusCode,
    method: &Method,
    retry_after_header: Option<&str>,
    body: &str,
) -> StorageError {
    match classify(status.as_u16()) {
        AzureStatus::Throttled => StorageError::Throttled {
            retry_after: retry_after_header.and_then(retry_after),
        },
        AzureStatus::Unauthorized => StorageError::Unauthorized,
        AzureStatus::Conflict | AzureStatus::PreconditionFailed => StorageError::Conflict,
        _ => StorageError::backend(format!(
            "Azure answered a {method} request with {status}: {}",
            body.trim()
        )),
    }
}

/// One page of a `List Blobs` response.
///
/// `Blobs` is required rather than defaulted, so a body that is not a listing — an error document
/// answered with a success status, a proxy's interstitial — fails the call instead of parsing into
/// an empty page and ending the pagination early.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ListBlobsOutput {
    /// The blobs in this page.
    pub(super) blobs: Blobs,
    /// The marker to resume from, empty or absent when the listing is complete.
    #[serde(default, rename = "NextMarker")]
    pub(super) next_marker: Option<String>,
}

impl ListBlobsOutput {
    /// The cursor to hand back, or `None` when the listing is complete.
    pub(super) fn cursor(&self) -> Option<String> {
        self.next_marker
            .as_ref()
            .filter(|marker| !marker.is_empty())
            .cloned()
    }
}

/// The `Blobs` element of a listing.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Blobs {
    /// One entry per blob.
    pub(super) blob: Vec<Blob>,
}

/// One blob in a listing.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct Blob {
    /// The blob's name, which is the key.
    pub(super) name: String,
    /// Its properties.
    pub(super) properties: BlobProperties,
}

/// The properties a listing reports for a blob.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct BlobProperties {
    /// The blob's size in bytes.
    #[serde(rename = "Content-Length")]
    pub(super) content_length: u64,
    /// Its last modification, as an RFC 2822 date.
    #[serde(rename = "Last-Modified")]
    pub(super) last_modified: String,
    /// Its content type.
    #[serde(rename = "Content-Type")]
    pub(super) content_type: String,
    /// Its entity tag, which a listing reports **unquoted**.
    pub(super) etag: String,
}

impl BlobProperties {
    /// The last modification as seconds since the Unix epoch.
    ///
    /// A date this client cannot read reports `None` rather than failing the whole listing: a
    /// timestamp is not what a caller paginates on.
    pub(super) fn last_modified_seconds(&self) -> Option<u64> {
        reqsign_core::time::Timestamp::parse_rfc2822(&self.last_modified)
            .ok()
            .and_then(|timestamp| u64::try_from(timestamp.as_second()).ok())
    }

    /// The entity tag in the quoted form an `ETag` header carries.
    ///
    /// A listing reports the tag bare (`0x8DA8BEB55D0EA35`) while the header form is quoted, and
    /// [`ObjectMetadata::etag`](skyzen_services::storage::ObjectMetadata::etag) documents the
    /// header form — so a tag read from a listing can be passed straight into an `If-None-Match`.
    pub(super) fn quoted_etag(&self) -> Option<String> {
        if self.etag.is_empty() {
            return None;
        }

        if self.etag.starts_with('"') || self.etag.starts_with("W/") {
            Some(self.etag.clone())
        } else {
            Some(format!("\"{}\"", self.etag))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BlobRest, Credential, ListBlobsOutput};
    use http::Method;

    /// A real `List Blobs` response, kept as a file so the fixture stays readable.
    const LIST_BLOBS: &str = include_str!("list_blobs.xml");

    /// The final page of a listing, whose `NextMarker` is empty.
    const LIST_BLOBS_LAST_PAGE: &str = include_str!("list_blobs_last_page.xml");

    fn rest() -> BlobRest {
        BlobRest::new(
            "https://skyzentest.blob.core.windows.net",
            "uploads",
            Credential::SharedKey {
                account_name: "skyzentest".to_owned(),
                account_key: "c2t5emVuLXRlc3Qta2V5".to_owned(),
            },
        )
        .expect("the endpoint should parse")
    }

    #[test]
    fn a_listing_page_is_parsed_into_keys_metadata_and_a_marker() {
        let output: ListBlobsOutput =
            quick_xml::de::from_str(LIST_BLOBS).expect("the listing should parse");

        let names: Vec<&str> = output
            .blobs
            .blob
            .iter()
            .map(|blob| blob.name.as_str())
            .collect();
        assert_eq!(names, vec!["photos/one.png", "photos/two.png"]);

        let first = &output.blobs.blob[0];
        assert_eq!(first.properties.content_length, 11);
        assert_eq!(first.properties.content_type, "image/png");
        // The listing carries a bare tag; the metadata this backend reports carries the HTTP form.
        assert_eq!(first.properties.etag, "0x8DA8BEB55D0EA35");
        assert_eq!(
            first.properties.quoted_etag().as_deref(),
            Some("\"0x8DA8BEB55D0EA35\"")
        );
        assert_eq!(
            first.properties.last_modified_seconds(),
            Some(1_662_017_209)
        );

        assert_eq!(output.cursor().as_deref(), Some("2!108!MDAwMDI0"));
    }

    #[test]
    fn the_last_page_of_a_listing_reports_no_cursor() {
        let output: ListBlobsOutput =
            quick_xml::de::from_str(LIST_BLOBS_LAST_PAGE).expect("the listing should parse");

        assert_eq!(output.blobs.blob.len(), 1);
        // An empty `<NextMarker />` means the listing is complete, not "resume from nothing".
        assert_eq!(output.cursor(), None);
    }

    #[test]
    fn a_listing_body_that_is_not_a_listing_is_refused() {
        assert!(
            quick_xml::de::from_str::<ListBlobsOutput>("<Error><Code>Nope</Code></Error>").is_err()
        );
    }

    #[test]
    fn a_blob_url_percent_encodes_each_segment_and_keeps_the_path() {
        let rest = rest();

        assert_eq!(
            rest.blob_url("photos/one.png").unwrap().as_str(),
            "https://skyzentest.blob.core.windows.net/uploads/photos/one.png"
        );
        assert_eq!(
            rest.blob_url("holiday photos/a b?c.png").unwrap().as_str(),
            "https://skyzentest.blob.core.windows.net/uploads/holiday%20photos/a%20b%3Fc.png"
        );
    }

    #[test]
    fn an_account_key_can_presign_and_a_shared_signature_cannot() {
        assert!(rest().can_presign());

        let sas = BlobRest::new(
            "https://skyzentest.blob.core.windows.net",
            "uploads",
            Credential::SasToken {
                token: "sv=2020-12-06&sig=signature".to_owned(),
            },
        )
        .expect("the endpoint should parse");
        assert!(!sas.can_presign());
    }

    #[tokio::test]
    async fn a_presigned_url_carries_the_signature_the_service_verifies() {
        let url = rest()
            .presign(
                "photos/one.png",
                &Method::GET,
                super::READ_PERMISSION,
                core::time::Duration::from_mins(15),
            )
            .await
            .expect("an account key should sign");

        assert!(url.starts_with("https://skyzentest.blob.core.windows.net/uploads/photos/one.png?"));
        // The query the service checks: version, expiry, permissions, resource and signature.
        for parameter in ["sv=", "se=", "sp=r", "sr=b", "sig="] {
            assert!(url.contains(parameter), "{url} should carry {parameter}");
        }
    }
}
