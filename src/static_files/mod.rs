//! Serving files: from disk, or embedded into the binary.
//!
//! Both servers speak the caching half of HTTP: they emit an `ETag`, answer `If-None-Match` and
//! `If-Modified-Since` with `304 Not Modified`, and serve a single `Range` as `206 Partial
//! Content` so media can be scrubbed. Both pick a precompressed sibling (`app.js.br`,
//! `app.js.zst`, `app.js.gz`) by the request's `Accept-Encoding` when one exists — the highest
//! quality wins, then the smallest representation — and say `Vary: Accept-Encoding` on every
//! response for such a file. `skyzen build` writes the siblings (`[static_files] precompress`).
//!
//! [`StaticDir`] keeps a per-thread cache of resolved files (see [`StaticCacheSettings`]): a file
//! below the operator's inline limit is held in memory as `Bytes`, a larger one as an open
//! descriptor that each response sends as a file region — `sendfile`/`splice` on the HTTP/1.1
//! server, through kernel TLS too where Linux has it.
//!
//! Two deliberate limits: a multi-range request is answered with the whole file and `200`, because
//! `multipart/byteranges` is rarely worth its complexity, and `If-Range` is not consulted — an
//! `If-None-Match` on the same request already covers the case it guards. Only the filesystem
//! server sends `Last-Modified`; an embedded file has no modification time, so it validates by
//! `ETag` alone.

use core::future::{ready, Future};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::{
    io,
    path::{Component, Path, PathBuf},
};

use base64::Engine;
use headers::{AcceptRanges, ContentRange, ETag, HeaderMapExt, LastModified, Range as RangeHeader};
use include_dir::{Dir, File};
use sha1::{Digest, Sha1};

use crate::{
    header::{self, HeaderMap, HeaderValue},
    routing::{IntoRouteNode, MethodFilter, Params, Route, RouteNode},
    Body, Endpoint, Method, Request, Response, StatusCode,
};

mod encoding;
use encoding::Coding;

#[cfg(not(target_arch = "wasm32"))]
mod cache;
#[cfg(not(target_arch = "wasm32"))]
mod disk;
#[cfg(not(target_arch = "wasm32"))]
mod fs;

#[cfg(not(target_arch = "wasm32"))]
pub use cache::StaticCacheSettings;
#[cfg(not(target_arch = "wasm32"))]
pub use disk::StaticDir;
#[cfg(not(target_arch = "wasm32"))]
pub use fs::{BlockingFs, CompioFs, FileSystem};

/// Mount embedded static files into the router.
///
/// `EmbeddedStaticDir` serves assets from a compile-time embedded [`Dir`]. A precompressed
/// sibling embedded with a file (`app.js.br` beside `app.js`) is served to a client that accepts
/// its coding, exactly as [`StaticDir`] does.
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
    /// Nothing is sent by default, which leaves caching to the validators. Embedded assets are
    /// usually built into a versioned binary, so a long `max-age` is the common choice.
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

fn route_node_for_static<E: Endpoint + Clone + 'static>(
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

impl Disposition {
    /// The bytes to send, as `(offset, length)`, or `None` when the response has no body.
    const fn span(&self, len: u64) -> Option<(u64, u64)> {
        match self {
            Self::NotModified | Self::Unsatisfiable => None,
            Self::Partial { start, end } => Some((*start, *end - *start + 1)),
            Self::Full => Some((0, len)),
        }
    }
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
    // `If-None-Match`/`If-Modified-Since` precedence lives in one shared evaluator —
    // `Conditional`, `Preconditions` and `AutoETag` follow the same rule.
    if crate::utils::conditional::fresh(headers, etag, last_modified) {
        return Disposition::NotModified;
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
    /// The coding of the representation chosen, `None` for the file itself.
    coding: Option<Coding>,
    /// Whether the file has a precompressed sibling, so the choice depended on `Accept-Encoding`.
    has_variants: bool,
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
        // On every status, a 304 included (RFC 9110 §15.4.5): a cache keys the stored response by
        // it.
        if self.has_variants {
            headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
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
                self.set_representation(headers);
            }
            Disposition::Full => {
                headers.typed_insert(AcceptRanges::bytes());
                self.set_representation(headers);
            }
        }

        response
    }

    fn set_representation(&self, headers: &mut HeaderMap) {
        if let Some(value) = &self.content_type {
            headers.insert(header::CONTENT_TYPE, value.clone());
        }
        if let Some(coding) = self.coding {
            headers.insert(header::CONTENT_ENCODING, coding.header_value());
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

/// The path `path` names below a mount, with `..`, absolute and NUL-bearing segments refused.
fn sanitize_relative_path(path: &str) -> Option<PathBuf> {
    if path.contains('\0') {
        return None;
    }
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

/// The sanitized path the request's `{*path}` capture names.
fn requested_path(request: &Request) -> Result<PathBuf, StaticDirError> {
    let requested = request
        .extensions()
        .get::<Params>()
        .and_then(|params| params.get("path").ok())
        .unwrap_or("");
    sanitize_relative_path(requested).ok_or(StaticDirError::InvalidPath)
}

fn guess_content_type(path: &Path) -> Option<HeaderValue> {
    mime_guess::from_path(path)
        .first_raw()
        .and_then(|mime| HeaderValue::from_str(mime).ok())
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
        let sanitized = requested_path(request)?;
        let file = resolve_embedded_file(self.directory, &sanitized, &self.index_file, self.spa)
            .ok_or(StaticDirError::FileNotFound)?;

        let variants: smallvec::SmallVec<[(Coding, &'static File<'static>); 3]> = Coding::ALL
            .into_iter()
            .filter_map(|coding| {
                let mut sibling = file.path().as_os_str().to_owned();
                sibling.push(".");
                sibling.push(coding.extension());
                self.directory
                    .get_file(Path::new(&sibling))
                    .map(|sibling| (coding, sibling))
            })
            .collect();
        let chosen = encoding::choose(
            request.headers(),
            file.contents().len() as u64,
            variants
                .iter()
                .map(|(coding, sibling)| (*coding, sibling.contents().len() as u64)),
        );
        let (coding, representation) = chosen.map_or((None, file), |index| {
            (Some(variants[index].0), variants[index].1)
        });

        let contents = representation.contents();
        let len = contents.len() as u64;
        let served = ServedFile {
            len,
            etag: self.etags.get(self.directory, representation),
            // An embedded file has no modification time; its ETag is the validator.
            last_modified: None,
            content_type: guess_content_type(file.path()),
            coding,
            has_variants: !variants.is_empty(),
        };

        let disposition = negotiate(request.headers(), served.etag.as_ref(), None, len);
        let mut response = served.head(&disposition, self.cache_control.as_ref());
        if let Some((offset, length)) = disposition.span(len) {
            let slice = &contents[usize_range(offset, length)];
            *response.body_mut() = Body::from_bytes(http_kit::utils::Bytes::from_static(slice));
        }
        Ok(response)
    }
}

/// `length` bytes from `offset`, as a slice range; the span came from [`negotiate`], which kept it
/// inside the file.
fn usize_range(offset: u64, length: u64) -> std::ops::Range<usize> {
    let start = usize::try_from(offset).expect("an in-memory file's offset fits in usize");
    let length = usize::try_from(length).expect("an in-memory file's length fits in usize");
    start..start + length
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

impl Endpoint for EmbeddedStaticDirEndpoint {
    type Error = StaticDirError;
    // The assets are compiled into the binary, so the future is ready on creation rather than an
    // `async` block with nothing to await.
    fn respond(
        &mut self,
        request: &mut Request,
    ) -> impl Future<Output = Result<Response, Self::Error>> {
        ready(self.serve(request))
    }
}

#[cfg(test)]
mod tests {
    use super::{negotiate, normalize_mount_path, sanitize_relative_path, Disposition};
    use crate::header::{self, HeaderValue};

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
        assert!(sanitize_relative_path("a\0b").is_none());
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
}
