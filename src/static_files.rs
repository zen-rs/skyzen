#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
use std::{
    io,
    path::{Component, Path, PathBuf},
};

use include_dir::{Dir, File};

use crate::{
    header::{self, HeaderValue},
    routing::{IntoRouteNode, MethodFilter, Params, Route, RouteNode},
    Endpoint, Method, Request, Response, StatusCode,
};
use skyzen_core::Extractor;

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
        }
    }

    /// Override the default file that is served when a directory (or the mount root) is requested.
    #[must_use]
    pub fn index_file(mut self, index_file: impl Into<String>) -> Self {
        self.index_file = index_file.into();
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
        }
    }

    /// Override the default file used for root and directory fallback.
    #[must_use]
    pub fn index_file(mut self, index_file: impl Into<String>) -> Self {
        self.index_file = index_file.into();
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

#[cfg(not(target_arch = "wasm32"))]
async fn serve_static(
    directory: &Path,
    index_file: &str,
    spa: bool,
    params: &Params,
) -> Result<Response, StaticDirError> {
    let requested_path = params.get("path").unwrap_or("");
    let sanitized = sanitize_relative_path(requested_path).ok_or(StaticDirError::InvalidPath)?;
    let file_path = resolve_target_path(directory, &sanitized, index_file, spa)
        .await
        .ok_or(StaticDirError::FileNotFound)?;

    let data = read_file(&file_path).await?;
    let mut response = Response::new(http_kit::Body::from(data));

    if let Some(value) = guess_content_type(&file_path) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }

    Ok(response)
}

#[cfg(not(target_arch = "wasm32"))]
async fn read_file(path: &Path) -> Result<Vec<u8>, StaticDirError> {
    async_fs::read(path).await.map_err(StaticDirError::IoError)
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
#[derive(Clone)]
struct StaticDirEndpoint {
    directory: Arc<PathBuf>,
    index_file: Arc<String>,
    spa: bool,
}

#[derive(Clone)]
struct EmbeddedStaticDirEndpoint {
    directory: &'static Dir<'static>,
    index_file: String,
    spa: bool,
}

fn serve_embedded_static(
    directory: &'static Dir<'static>,
    index_file: &str,
    spa: bool,
    params: &Params,
) -> Result<Response, StaticDirError> {
    let requested_path = params.get("path").unwrap_or("");
    let sanitized = sanitize_relative_path(requested_path).ok_or(StaticDirError::InvalidPath)?;

    let embedded_file = resolve_embedded_file(directory, &sanitized, index_file, spa)
        .ok_or(StaticDirError::FileNotFound)?;

    let mut response = Response::new(http_kit::Body::from(embedded_file.contents().to_vec()));
    if let Some(value) = guess_embedded_content_type(embedded_file) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }

    Ok(response)
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
        let params = Params::extract(request).await.unwrap(); // Params extractor never fails, so unwrap is safe
        serve_static(
            self.directory.as_ref(),
            self.index_file.as_ref(),
            self.spa,
            &params,
        )
        .await
    }
}

impl Endpoint for EmbeddedStaticDirEndpoint {
    type Error = StaticDirError;
    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        let params = Params::extract(request).await.unwrap(); // Params extractor never fails, so unwrap is safe
        serve_embedded_static(self.directory, &self.index_file, self.spa, &params)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{normalize_mount_path, sanitize_relative_path};
    use crate::{
        header,
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
        let body = response.into_body().into_bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"body { color: #fff; }");
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
