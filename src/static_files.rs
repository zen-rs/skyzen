//! Serving files: from disk, or embedded into the binary.
//!
//! Both servers stream rather than buffer, and both speak the caching half of HTTP: they emit an
//! `ETag`, answer `If-None-Match` and `If-Modified-Since` with `304 Not Modified`, and serve a
//! single `Range` as `206 Partial Content` so media can be scrubbed.
//!
//! Two deliberate limits: a multi-range request is answered with the whole file and `200`, because
//! `multipart/byteranges` is rarely worth its complexity, and `If-Range` is not consulted — an
//! `If-None-Match` on the same request already covers the case it guards. Only the filesystem
//! server sends `Last-Modified`; an embedded file has no modification time, so it validates by
//! `ETag` alone.

use core::future::{ready, Future};
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::{
    io,
    path::{Component, Path, PathBuf},
};

use base64::Engine;
use headers::{
    AcceptRanges, ContentRange, ETag, HeaderMapExt, IfModifiedSince, IfNoneMatch, LastModified,
    Range as RangeHeader,
};
use include_dir::{Dir, File};
use sha1::{Digest, Sha1};

use crate::{
    header::{self, HeaderMap, HeaderValue},
    routing::{IntoRouteNode, MethodFilter, Params, Route, RouteNode},
    Body, Endpoint, Method, Request, Response, StatusCode,
};

/// Mount a directory tree into the router.
///
/// `StaticDir` implements [`IntoRouteNode`], so it can be dropped directly inside `Route::new`.
/// Files are looked up relative to the provided directory, `..` segments are rejected,
/// and directories fall back to `index.html` by default.
///
/// SPA (Single Page Application) routing is **not** enabled by default. Call [`.spa()`](Self::spa)
/// to enable it — extensionless paths that don't match a file will then fall back to the index
/// file, while requests for missing assets (paths with a file extension) still return 404.
///
/// Note: `StaticDir` does not support `OpenAPI` documentation generation for its routes.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct StaticDir {
    mount_path: String,
    directory: Arc<PathBuf>,
    index_file: String,
    spa: bool,
    cache_control: Option<HeaderValue>,
}

#[cfg(not(target_arch = "wasm32"))]
impl StaticDir {
    /// Create a new static directory handler mounted at `mount_path`.
    ///
    /// The path may be provided without a leading slash (`"assets"`); it will be normalized to `/assets`.
    #[must_use]
    pub fn new(mount_path: impl Into<String>, directory: impl Into<PathBuf>) -> Self {
        let mount_path_string = mount_path.into();
        Self {
            mount_path: normalize_mount_path(&mount_path_string),
            directory: Arc::new(directory.into()),
            index_file: "index.html".to_owned(),
            spa: false,
            cache_control: None,
        }
    }

    /// Override the default file that is served when a directory (or the mount root) is requested.
    #[must_use]
    pub fn index_file(mut self, index_file: impl Into<String>) -> Self {
        self.index_file = index_file.into();
        self
    }

    /// Send `value` as the `Cache-Control` header on every file served.
    ///
    /// Nothing is sent by default, which leaves caching to the validators (`ETag` and
    /// `Last-Modified`): correct, but it costs a conditional request per asset. Fingerprinted
    /// assets should say `public, max-age=31536000, immutable` instead.
    ///
    /// ```rust
    /// # #[cfg(not(target_arch = "wasm32"))] {
    /// use skyzen::{header::HeaderValue, static_files::StaticDir};
    ///
    /// let assets = StaticDir::new("/assets", "./dist")
    ///     .cache_control(HeaderValue::from_static("public, max-age=31536000, immutable"));
    /// # }
    /// ```
    #[must_use]
    pub fn cache_control(mut self, value: HeaderValue) -> Self {
        self.cache_control = Some(value);
        self
    }

    /// Enable SPA (Single Page Application) routing.
    ///
    /// When enabled, requests for extensionless paths that don't match a file or directory
    /// will fall back to the root index file. Requests for missing assets (paths with a file
    /// extension like `.js`, `.css`, `.png`) still return 404.
    #[must_use]
    pub const fn spa(mut self) -> Self {
        self.spa = true;
        self
    }
}

/// Mount embedded static files into the router.
///
/// `EmbeddedStaticDir` serves assets from a compile-time embedded [`Dir`].
///
/// SPA (Single Page Application) routing is **not** enabled by default. Call [`.spa()`](Self::spa)
/// to enable it — extensionless paths that don't match a file will then fall back to the index
/// file, while requests for missing assets (paths with a file extension) still return 404.
#[derive(Debug, Clone)]
pub struct EmbeddedStaticDir {
    mount_path: String,
    directory: &'static Dir<'static>,
    index_file: String,
    spa: bool,
    cache_control: Option<HeaderValue>,
}

impl EmbeddedStaticDir {
    /// Create an embedded static handler mounted at `mount_path`.
    #[must_use]
    pub fn new(mount_path: impl Into<String>, directory: &'static Dir<'static>) -> Self {
        let mount_path_string = mount_path.into();
        Self {
            mount_path: normalize_mount_path(&mount_path_string),
            directory,
            index_file: "index.html".to_owned(),
            spa: false,
            cache_control: None,
        }
    }

    /// Override the default file used for root and directory fallback.
    #[must_use]
    pub fn index_file(mut self, index_file: impl Into<String>) -> Self {
        self.index_file = index_file.into();
        self
    }

    /// Send `value` as the `Cache-Control` header on every file served.
    ///
    /// See [`StaticDir::cache_control`]; embedded assets are usually built into a versioned
    /// binary, so a long `max-age` is the common choice.
    #[must_use]
    pub fn cache_control(mut self, value: HeaderValue) -> Self {
        self.cache_control = Some(value);
        self
    }

    /// Enable SPA (Single Page Application) routing.
    ///
    /// When enabled, requests for extensionless paths that don't match a file or directory
    /// will fall back to the root index file. Requests for missing assets (paths with a file
    /// extension like `.js`, `.css`, `.png`) still return 404.
    #[must_use]
    pub const fn spa(mut self) -> Self {
        self.spa = true;
        self
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl IntoRouteNode for StaticDir {
    fn into_route_node(self) -> RouteNode {
        let endpoint = StaticDirEndpoint {
            directory: self.directory.clone(),
            index_file: Arc::new(self.index_file.clone()),
            spa: self.spa,
            cache_control: self.cache_control,
        };
        route_node_for_static(self.mount_path, endpoint)
    }
}

impl IntoRouteNode for EmbeddedStaticDir {
    fn into_route_node(self) -> RouteNode {
        let endpoint = EmbeddedStaticDirEndpoint {
            directory: self.directory,
            index_file: self.index_file,
            spa: self.spa,
            cache_control: self.cache_control,
            // Shared, because the endpoint is cloned into two registrations (the mount point and
            // the wildcard below it) and both must consult the same memo.
            etags: EtagCache::default(),
        };
        route_node_for_static(self.mount_path, endpoint)
    }
}

fn route_node_for_static<E: Endpoint + Clone + Send + Sync + 'static>(
    mount_path: String,
    endpoint: E,
) -> RouteNode
where
    E::Error: crate::HttpError,
{
    let wildcard_suffix = if mount_path == "/" {
        "{*path}"
    } else {
        "/{*path}"
    };
    let route = Route::new((
        RouteNode::new_endpoint(
            "",
            MethodFilter::Exact(Method::GET),
            endpoint.clone(),
            None,
            Vec::new(),
        ),
        RouteNode::new_endpoint(
            wildcard_suffix,
            MethodFilter::Exact(Method::GET),
            endpoint,
            None,
            Vec::new(),
        ),
    ));

    RouteNode::new_route(mount_path, route)
}

/// What the request's conditional and range headers ask for.
#[derive(Debug, PartialEq, Eq)]
enum Disposition {
    /// The client's copy is still good.
    NotModified,
    /// A `Range` was asked for that this file cannot satisfy.
    Unsatisfiable,
    /// One byte range, inclusive at both ends.
    Partial { start: u64, end: u64 },
    /// The whole file.
    Full,
}

/// Decide what to send, from the request's validators and range.
///
/// Validators win over the range: a client whose cached copy is current gets `304` and no body,
/// whatever byte range it asked for.
fn negotiate(
    headers: &HeaderMap,
    etag: Option<&ETag>,
    last_modified: Option<SystemTime>,
    len: u64,
) -> Disposition {
    if let (Some(if_none_match), Some(etag)) = (headers.typed_get::<IfNoneMatch>(), etag) {
        if !if_none_match.precondition_passes(etag) {
            return Disposition::NotModified;
        }
    } else if let (Some(if_modified_since), Some(modified)) =
        (headers.typed_get::<IfModifiedSince>(), last_modified)
    {
        // `If-Modified-Since` is only consulted when the stronger validator did not settle it,
        // which is what RFC 9110 prescribes.
        if !if_modified_since.is_modified(modified) {
            return Disposition::NotModified;
        }
    }

    let Some(range) = headers.typed_get::<RangeHeader>() else {
        return Disposition::Full;
    };

    let mut satisfiable = range.satisfiable_ranges(len);
    let Some(first) = satisfiable.next() else {
        return Disposition::Unsatisfiable;
    };
    if satisfiable.next().is_some() {
        // Multi-range: answered in full, as documented on the module.
        return Disposition::Full;
    }

    let start = match first.0 {
        std::ops::Bound::Included(start) => start,
        std::ops::Bound::Excluded(start) => start.saturating_add(1),
        std::ops::Bound::Unbounded => 0,
    };
    let end = match first.1 {
        std::ops::Bound::Included(end) => end.min(len.saturating_sub(1)),
        std::ops::Bound::Excluded(end) => end.saturating_sub(1).min(len.saturating_sub(1)),
        std::ops::Bound::Unbounded => len.saturating_sub(1),
    };

    if len == 0 || start > end || start >= len {
        return Disposition::Unsatisfiable;
    }
    Disposition::Partial { start, end }
}

/// Everything the response needs that does not depend on where the bytes come from.
struct ServedFile {
    len: u64,
    etag: Option<ETag>,
    last_modified: Option<SystemTime>,
    content_type: Option<HeaderValue>,
}

impl ServedFile {
    /// Build the response head — status, validators and caching headers — for `disposition`.
    fn head(&self, disposition: &Disposition, cache_control: Option<&HeaderValue>) -> Response {
        let mut response = Response::new(Body::empty());
        let status = match disposition {
            Disposition::NotModified => StatusCode::NOT_MODIFIED,
            Disposition::Unsatisfiable => StatusCode::RANGE_NOT_SATISFIABLE,
            Disposition::Partial { .. } => StatusCode::PARTIAL_CONTENT,
            Disposition::Full => StatusCode::OK,
        };
        *response.status_mut() = status;

        let headers = response.headers_mut();
        if let Some(etag) = &self.etag {
            headers.typed_insert(etag.clone());
        }
        if let Some(modified) = self.last_modified {
            headers.typed_insert(LastModified::from(modified));
        }
        if let Some(value) = cache_control {
            headers.insert(header::CACHE_CONTROL, value.clone());
        }

        match disposition {
            // A 304 repeats the validators and nothing else; a 416 describes the file's size and
            // carries no content type of its own.
            Disposition::NotModified => {}
            Disposition::Unsatisfiable => {
                headers.typed_insert(ContentRange::unsatisfied_bytes(self.len));
            }
            Disposition::Partial { start, end } => {
                headers.typed_insert(AcceptRanges::bytes());
                if let Ok(content_range) = ContentRange::bytes(*start..=*end, self.len) {
                    headers.typed_insert(content_range);
                }
                self.set_content_type(headers);
            }
            Disposition::Full => {
                headers.typed_insert(AcceptRanges::bytes());
                self.set_content_type(headers);
            }
        }

        response
    }

    fn set_content_type(&self, headers: &mut HeaderMap) {
        if let Some(value) = &self.content_type {
            headers.insert(header::CONTENT_TYPE, value.clone());
        }
    }
}

/// The `ETag` for a byte slice: a SHA-1 of the content, which is what an embedded file has
/// instead of a modification time.
fn content_etag(contents: &[u8]) -> Option<ETag> {
    let digest = Sha1::digest(contents);
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    ["\"", &encoded, "\""].concat().parse().ok()
}

/// The `ETag` for a filesystem entry: its size and modification time, which change together
/// whenever the content does and cost no read to obtain.
#[cfg(not(target_arch = "wasm32"))]
fn metadata_etag(len: u64, modified: Option<SystemTime>) -> Option<ETag> {
    let modified = modified?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    format!("\"{len:x}-{modified:x}\"").parse().ok()
}

/// Lazily hashed `ETag`s for an embedded directory, computed once for the whole tree.
///
/// An embedded file's bytes never change, so the map is built on the first request and shared by
/// every clone of the endpoint from then on.
#[derive(Debug, Default, Clone)]
struct EtagCache(std::sync::Arc<OnceLock<HashMap<&'static Path, Option<ETag>>>>);

impl EtagCache {
    fn get(&self, directory: &'static Dir<'static>, file: &File<'static>) -> Option<ETag> {
        self.0
            .get_or_init(|| {
                let mut map = HashMap::new();
                collect_etags(directory, &mut map);
                map
            })
            .get(file.path())
            .cloned()
            .flatten()
    }
}

fn collect_etags(directory: &'static Dir<'static>, map: &mut HashMap<&'static Path, Option<ETag>>) {
    for file in directory.files() {
        map.insert(file.path(), content_etag(file.contents()));
    }
    for nested in directory.dirs() {
        collect_etags(nested, map);
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn serve_static(
    directory: &Path,
    index_file: &str,
    spa: bool,
    cache_control: Option<&HeaderValue>,
    request: &Request,
) -> Result<Response, StaticDirError> {
    let params = request
        .extensions()
        .get::<Params>()
        .cloned()
        .unwrap_or_else(|| Params::new(Vec::new()));
    let requested_path = params.get("path").unwrap_or("");
    let sanitized = sanitize_relative_path(requested_path).ok_or(StaticDirError::InvalidPath)?;
    let file_path = resolve_target_path(directory, &sanitized, index_file, spa)
        .await
        .ok_or(StaticDirError::FileNotFound)?;

    let metadata = async_fs::metadata(&file_path)
        .await
        .map_err(StaticDirError::IoError)?;
    let len = metadata.len();
    let modified = metadata.modified().ok();

    let served = ServedFile {
        len,
        etag: metadata_etag(len, modified),
        last_modified: modified,
        content_type: guess_content_type(&file_path),
    };

    let disposition = negotiate(request.headers(), served.etag.as_ref(), modified, len);
    let mut response = served.head(&disposition, cache_control);

    match disposition {
        Disposition::NotModified | Disposition::Unsatisfiable => {}
        Disposition::Partial { start, end } => {
            let length = end - start + 1;
            *response.body_mut() = file_body(&file_path, start, length).await?;
        }
        Disposition::Full => {
            *response.body_mut() = file_body(&file_path, 0, len).await?;
        }
    }

    Ok(response)
}

/// Stream `length` bytes of `path` starting at `start`, without buffering the file.
#[cfg(not(target_arch = "wasm32"))]
async fn file_body(path: &Path, start: u64, length: u64) -> Result<Body, StaticDirError> {
    use http_kit::utils::io::{AsyncReadExt, AsyncSeekExt, BufReader, SeekFrom};

    let mut file = async_fs::File::open(path)
        .await
        .map_err(StaticDirError::IoError)?;
    if start > 0 {
        file.seek(SeekFrom::Start(start))
            .await
            .map_err(StaticDirError::IoError)?;
    }
    let reader = BufReader::new(file.take(length));
    Ok(Body::from_reader(
        reader,
        usize::try_from(length).unwrap_or(usize::MAX),
    ))
}

#[cfg(not(target_arch = "wasm32"))]
fn guess_content_type(path: &Path) -> Option<HeaderValue> {
    mime_guess::from_path(path)
        .first_raw()
        .and_then(|mime| HeaderValue::from_str(mime).ok())
}

#[cfg(not(target_arch = "wasm32"))]
async fn resolve_target_path(
    base: &Path,
    relative: &Path,
    index_file: &str,
    spa: bool,
) -> Option<PathBuf> {
    let target = if relative.as_os_str().is_empty() {
        base.to_path_buf()
    } else {
        base.join(relative)
    };

    if let Ok(metadata) = async_fs::metadata(&target).await {
        let resolved = if metadata.is_dir() {
            target.join(index_file)
        } else {
            target
        };
        if async_fs::metadata(&resolved)
            .await
            .is_ok_and(|m| m.is_file())
        {
            return Some(resolved);
        }
    }

    // SPA fallback: only for extensionless paths
    if spa && relative.extension().is_none() {
        let fallback = base.join(index_file);
        if async_fs::metadata(&fallback)
            .await
            .is_ok_and(|m| m.is_file())
        {
            return Some(fallback);
        }
    }

    None
}

fn sanitize_relative_path(path: &str) -> Option<PathBuf> {
    let mut buf = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(segment) => buf.push(segment),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    Some(buf)
}

fn normalize_mount_path(mount_path: &str) -> String {
    let mut normalized = mount_path.trim().to_owned();
    if normalized.is_empty() {
        return "/".to_owned();
    }
    if !normalized.starts_with('/') {
        normalized.insert(0, '/');
    }
    if normalized.ends_with('/') && normalized.len() > 1 {
        while normalized.ends_with('/') && normalized.len() > 1 {
            normalized.pop();
        }
    }
    normalized
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
struct StaticDirEndpoint {
    directory: Arc<PathBuf>,
    index_file: Arc<String>,
    spa: bool,
    cache_control: Option<HeaderValue>,
}

#[derive(Debug, Clone)]
struct EmbeddedStaticDirEndpoint {
    directory: &'static Dir<'static>,
    index_file: String,
    spa: bool,
    cache_control: Option<HeaderValue>,
    etags: EtagCache,
}

impl EmbeddedStaticDirEndpoint {
    fn serve(&self, request: &Request) -> Result<Response, StaticDirError> {
        let params = request
            .extensions()
            .get::<Params>()
            .cloned()
            .unwrap_or_else(|| Params::new(Vec::new()));
        let requested_path = params.get("path").unwrap_or("");
        let sanitized =
            sanitize_relative_path(requested_path).ok_or(StaticDirError::InvalidPath)?;

        let file = resolve_embedded_file(self.directory, &sanitized, &self.index_file, self.spa)
            .ok_or(StaticDirError::FileNotFound)?;

        let contents = file.contents();
        let len = contents.len() as u64;
        let served = ServedFile {
            len,
            etag: self.etags.get(self.directory, file),
            // An embedded file has no modification time; its ETag is the validator.
            last_modified: None,
            content_type: guess_embedded_content_type(file),
        };

        let disposition = negotiate(request.headers(), served.etag.as_ref(), None, len);
        let mut response = served.head(&disposition, self.cache_control.as_ref());

        match disposition {
            Disposition::NotModified | Disposition::Unsatisfiable => {}
            Disposition::Partial { start, end } => {
                let slice = &contents[usize_range(start, end, contents.len())];
                *response.body_mut() = Body::from_bytes(http_kit::utils::Bytes::from_static(slice));
            }
            Disposition::Full => {
                *response.body_mut() =
                    Body::from_bytes(http_kit::utils::Bytes::from_static(contents));
            }
        }

        Ok(response)
    }
}

/// Clamp an inclusive byte range to a slice's bounds.
fn usize_range(start: u64, end: u64, len: usize) -> std::ops::Range<usize> {
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(len);
    let end = usize::try_from(end).unwrap_or(usize::MAX).min(len - 1);
    start..end + 1
}

fn resolve_embedded_file<'a>(
    directory: &'a Dir<'a>,
    relative: &Path,
    index_file: &str,
    spa: bool,
) -> Option<&'a File<'a>> {
    // Root request → serve index file
    if relative.as_os_str().is_empty() {
        return directory.get_file(index_file);
    }

    let relative_str = relative.to_string_lossy();

    // Exact file match
    if let Some(file) = directory.get_file(relative_str.as_ref()) {
        return Some(file);
    }

    // Directory with index file inside it
    if directory.get_dir(relative_str.as_ref()).is_some() {
        let nested_index = format!("{relative_str}/{index_file}");
        if let Some(file) = directory.get_file(&nested_index) {
            return Some(file);
        }
    }

    // SPA fallback: only for extensionless paths
    if spa && relative.extension().is_none() {
        return directory.get_file(index_file);
    }

    None
}

fn guess_embedded_content_type(file: &File<'_>) -> Option<HeaderValue> {
    mime_guess::from_path(file.path())
        .first_raw()
        .and_then(|mime| HeaderValue::from_str(mime).ok())
}

/// Errors that can occur when serving static files.
#[skyzen::error]
pub enum StaticDirError {
    /// The requested path is invalid.
    #[error("Invalid static path", status = StatusCode::BAD_REQUEST)]
    InvalidPath,
    /// The requested file was not found.
    #[error("File not found", status = StatusCode::NOT_FOUND)]
    FileNotFound,
    /// An I/O error occurred while reading the file.
    #[error("Failed to read file: {0}", status = StatusCode::INTERNAL_SERVER_ERROR)]
    IoError(#[from] io::Error),
}

#[cfg(not(target_arch = "wasm32"))]
impl Endpoint for StaticDirEndpoint {
    type Error = StaticDirError;
    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        serve_static(
            self.directory.as_ref(),
            self.index_file.as_ref(),
            self.spa,
            self.cache_control.as_ref(),
            request,
        )
        .await
    }
}

impl Endpoint for EmbeddedStaticDirEndpoint {
    type Error = StaticDirError;
    // The assets are compiled into the binary, so the future is ready on creation rather than an
    // `async` block with nothing to await.
    fn respond(
        &mut self,
        request: &mut Request,
    ) -> impl Future<Output = Result<Response, Self::Error>> + Send {
        ready(self.serve(request))
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{negotiate, normalize_mount_path, sanitize_relative_path, Disposition};
    use crate::{
        header::{self, HeaderValue},
        routing::{build, Route},
        static_files::StaticDir,
        Body, Method, StatusCode,
    };

    #[test]
    fn normalizes_mount_paths() {
        assert_eq!(normalize_mount_path("assets"), "/assets");
        assert_eq!(normalize_mount_path("/assets/"), "/assets");
        assert_eq!(normalize_mount_path("/"), "/");
    }

    #[test]
    fn rejects_parent_dirs() {
        assert!(sanitize_relative_path("../secrets").is_none());
        assert!(sanitize_relative_path("styles/../../etc").is_none());
        assert!(sanitize_relative_path("/absolute/path").is_none());
    }

    #[test]
    fn keeps_valid_relative_segments() {
        let path = sanitize_relative_path("styles/main.css").unwrap();
        assert_eq!(path, std::path::Path::new("styles").join("main.css"));
    }

    #[test]
    fn an_unsatisfiable_range_is_recognised() {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=100-200"));
        assert_eq!(
            negotiate(&headers, None, None, 10),
            Disposition::Unsatisfiable
        );
    }

    #[test]
    fn a_multi_range_request_is_answered_in_full() {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-1,4-5"));
        assert_eq!(negotiate(&headers, None, None, 10), Disposition::Full);
    }

    fn get_request(path: &str) -> http_kit::Request {
        let mut request = http_kit::Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("invalid path");
        *request.method_mut() = Method::GET;
        request
    }

    #[tokio::test]
    async fn serves_files_from_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("assets");
        std::fs::create_dir_all(&nested).unwrap();
        let css_path = nested.join("main.css");
        std::fs::write(&css_path, b"body { color: #fff; }").unwrap();

        let router = build(Route::new((StaticDir::new("/static", dir.path()),))).unwrap();

        let request = get_request("/static/assets/main.css");
        let response = router.clone().go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let header_value = response
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("missing content type");
        assert_eq!(header_value.to_str().unwrap(), "text/css");
        assert!(response.headers().contains_key(header::ETAG));
        assert!(response.headers().contains_key(header::LAST_MODIFIED));
        assert_eq!(
            response.headers().get(header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
        let body = response.into_body().into_bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"body { color: #fff; }");
    }

    #[tokio::test]
    async fn a_matching_etag_answers_304_without_a_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.js"), b"console.log(1)").unwrap();
        let router = build(Route::new((StaticDir::new("/s", dir.path()),))).unwrap();

        let first = router.clone().go(get_request("/s/app.js")).await.unwrap();
        let etag = first.headers().get(header::ETAG).unwrap().clone();

        let mut request = get_request("/s/app.js");
        request.headers_mut().insert(header::IF_NONE_MATCH, etag);
        let response = router.clone().go(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        let body = response.into_body().into_bytes().await.unwrap();
        assert!(body.is_empty(), "a 304 carries no body");
    }

    #[tokio::test]
    async fn a_not_modified_since_request_answers_304() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.js"), b"console.log(1)").unwrap();
        let router = build(Route::new((StaticDir::new("/s", dir.path()),))).unwrap();

        let first = router.clone().go(get_request("/s/app.js")).await.unwrap();
        let modified = first.headers().get(header::LAST_MODIFIED).unwrap().clone();

        let mut request = get_request("/s/app.js");
        request
            .headers_mut()
            .insert(header::IF_MODIFIED_SINCE, modified);
        let response = router.clone().go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn a_single_range_answers_206_with_just_those_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"0123456789").unwrap();
        let router = build(Route::new((StaticDir::new("/s", dir.path()),))).unwrap();

        let mut request = get_request("/s/data.bin");
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=2-5"));
        let response = router.clone().go(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 2-5/10"
        );
        let body = response.into_body().into_bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"2345");
    }

    #[tokio::test]
    async fn a_range_past_the_end_answers_416() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"0123456789").unwrap();
        let router = build(Route::new((StaticDir::new("/s", dir.path()),))).unwrap();

        let mut request = get_request("/s/data.bin");
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=50-60"));
        let response = router.clone().go(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes */10"
        );
    }

    #[tokio::test]
    async fn a_configured_cache_control_reaches_the_response() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.js"), b"console.log(1)").unwrap();
        let router = build(Route::new((StaticDir::new("/s", dir.path())
            .cache_control(HeaderValue::from_static("public, max-age=60")),)))
        .unwrap();

        let response = router.clone().go(get_request("/s/app.js")).await.unwrap();
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=60"
        );
    }

    #[tokio::test]
    async fn serves_index_file_for_root_requests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>Home</h1>").unwrap();

        let router = build(Route::new((StaticDir::new("/public", dir.path()),))).unwrap();

        let request = get_request("/public");
        let response = router.clone().go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "<h1>Home</h1>");
    }

    #[tokio::test]
    async fn blocks_path_traversal_attempts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"root").unwrap();

        let router = build(Route::new((StaticDir::new("/files", dir.path()),))).unwrap();

        let request = get_request("/files/../Cargo.toml");
        let error = router.clone().go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn returns_not_found_for_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let router = build(Route::new((StaticDir::new("/assets", dir.path()),))).unwrap();

        let request = get_request("/assets/app.js");
        let error = router.clone().go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn honors_custom_index_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("home.htm"), b"custom").unwrap();
        let router = build(Route::new((
            StaticDir::new("/web", dir.path()).index_file("home.htm"),
        )))
        .unwrap();

        let request = get_request("/web");
        let response = router.clone().go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "custom");
    }

    #[tokio::test]
    async fn spa_fallback_serves_index_for_extensionless_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>SPA</h1>").unwrap();

        let router = build(Route::new((StaticDir::new("/app", dir.path()).spa(),))).unwrap();

        let request = get_request("/app/about");
        let response = router.clone().go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "<h1>SPA</h1>");
    }

    #[tokio::test]
    async fn spa_returns_404_for_missing_files_with_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>SPA</h1>").unwrap();

        let router = build(Route::new((StaticDir::new("/app", dir.path()).spa(),))).unwrap();

        let request = get_request("/app/missing.js");
        let error = router.clone().go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn non_spa_returns_404_for_extensionless_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>Home</h1>").unwrap();

        let router = build(Route::new((StaticDir::new("/app", dir.path()),))).unwrap();

        let request = get_request("/app/about");
        let error = router.clone().go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
    }
}
