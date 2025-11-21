use std::{
    io,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use crate::{
    header::{self, HeaderValue},
    routing::{IntoRouteNode, Params, Route, RouteNode},
    Endpoint, Method, Request, Response, StatusCode,
};
use skyzen_core::Extractor;

/// Mount a directory tree into the router.
///
/// `StaticDir` implements [`IntoRouteNode`], so it can be dropped directly inside `Route::new`.
/// Files are looked up relative to the provided directory, `..` segments are rejected,
/// and directories fall back to `index.html` by default.
///
/// Note: `StaticDir` does not support `OpenAPI` documentation generation for its routes.
#[derive(Debug, Clone)]
pub struct StaticDir {
    mount_path: String,
    directory: Arc<PathBuf>,
    index_file: String,
}

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
        }
    }

    /// Override the default file that is served when a directory (or the mount root) is requested.
    #[must_use]
    pub fn index_file(mut self, index_file: impl Into<String>) -> Self {
        self.index_file = index_file.into();
        self
    }
}

impl IntoRouteNode for StaticDir {
    fn into_route_node(self) -> RouteNode {
        let endpoint = StaticDirEndpoint {
            directory: self.directory.clone(),
            index_file: Arc::new(self.index_file.clone()),
        };
        let wildcard_suffix = if self.mount_path == "/" {
            "{*path}"
        } else {
            "/{*path}"
        };
        let route = Route::new((
            RouteNode::new_endpoint("", Method::GET, endpoint.clone(), None),
            RouteNode::new_endpoint(wildcard_suffix, Method::GET, endpoint, None),
        ));

        RouteNode::new_route(self.mount_path, route)
    }
}

async fn serve_static(
    directory: &Path,
    index_file: &str,
    params: &Params,
) -> Result<Response, StaticDirError> {
    let requested_path = params.get("path").unwrap_or("");
    let sanitized = sanitize_relative_path(requested_path).ok_or(StaticDirError::InvalidPath)?;
    let file_path = resolve_target_path(directory, &sanitized, index_file)
        .ok_or(StaticDirError::FileNotFound)?;

    let data = read_file(&file_path).await?;
    let mut response = Response::new(http_kit::Body::from(data));

    if let Some(value) = guess_content_type(&file_path) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }

    Ok(response)
}

async fn read_file(path: &Path) -> Result<Vec<u8>, StaticDirError> {
    async_fs::read(path).await.map_err(StaticDirError::IoError)
}

fn guess_content_type(path: &Path) -> Option<HeaderValue> {
    mime_guess::from_path(path)
        .first_raw()
        .and_then(|mime| HeaderValue::from_str(mime).ok())
}

fn resolve_target_path(base: &Path, relative: &Path, index_file: &str) -> Option<PathBuf> {
    let target = if relative.as_os_str().is_empty() {
        base.to_path_buf()
    } else {
        base.join(relative)
    };

    let metadata = std::fs::metadata(&target).ok()?;
    let resolved = if metadata.is_dir() {
        target.join(index_file)
    } else {
        target
    };

    std::fs::metadata(&resolved)
        .ok()
        .and_then(|meta| if meta.is_file() { Some(resolved) } else { None })
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

#[derive(Clone)]
struct StaticDirEndpoint {
    directory: Arc<PathBuf>,
    index_file: Arc<String>,
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

impl Endpoint for StaticDirEndpoint {
    type Error = StaticDirError;
    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        let params = Params::extract(request).await.unwrap(); // Params extractor never fails, so unwrap is safe
        serve_static(self.directory.as_ref(), self.index_file.as_ref(), &params).await
    }
}

#[cfg(test)]
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
        assert_eq!(path.to_string_lossy(), "styles/main.css");
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
        assert_eq!(error.status(), Some(StatusCode::BAD_REQUEST));
    }

    #[tokio::test]
    async fn returns_not_found_for_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let router = build(Route::new((StaticDir::new("/assets", dir.path()),))).unwrap();

        let request = get_request("/assets/app.js");
        let error = router.clone().go(request).await.unwrap_err();
        assert_eq!(error.status(), Some(StatusCode::NOT_FOUND));
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
}
