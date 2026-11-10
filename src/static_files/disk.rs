//! [`StaticDir`]: files served from a directory on disk.

use std::{
    io,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::SystemTime,
};

use bytes::Bytes;
use headers::ETag;
use smallvec::SmallVec;

use super::{
    cache::{DirCache, StaticCacheSettings},
    encoding::{self, Coding},
    fs::{CompioFs, Entry, FileSystem, Opened},
    guess_content_type, negotiate, normalize_mount_path, requested_path, route_node_for_static,
    ServedFile, StaticDirError,
};
use crate::{
    header::HeaderValue,
    routing::{IntoRouteNode, RouteNode},
    Body, Endpoint, Request, Response,
};

/// Mount a directory tree into the router.
///
/// `StaticDir` implements [`IntoRouteNode`], so it can be dropped directly inside `Route::new`.
/// Files are looked up relative to the provided directory, `..` segments are rejected,
/// and directories fall back to `index.html` by default.
///
/// Each serving thread's router keeps a cache of resolved files, invalidated by filesystem
/// notifications and sized by [`StaticCacheSettings`]. Files are read through `F`, the
/// [`FileSystem`]: [`CompioFs`] by default, which is what `#[skyzen::main]` serves on; a router
/// driven by another executor takes [`BlockingFs`](super::BlockingFs) through
/// [`filesystem`](Self::filesystem).
///
/// SPA (Single Page Application) routing is **not** enabled by default. Call [`.spa()`](Self::spa)
/// to enable it — extensionless paths that don't match a file will then fall back to the index
/// file, while requests for missing assets (paths with a file extension) still return 404.
///
/// Note: `StaticDir` does not support `OpenAPI` documentation generation for its routes.
///
/// # Panics
///
/// Building the router panics when the directory cannot be watched for changes — it does not
/// exist, or the kernel's watch limit is reached — since its cache would then serve stale files.
#[derive(Debug, Clone)]
pub struct StaticDir<F = CompioFs> {
    mount_path: String,
    directory: Arc<PathBuf>,
    index_file: String,
    spa: bool,
    cache_control: Option<HeaderValue>,
    settings: StaticCacheSettings,
    fs: F,
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
            spa: false,
            cache_control: None,
            settings: StaticCacheSettings::default(),
            fs: CompioFs,
        }
    }
}

impl<F> StaticDir<F> {
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

    /// Size the per-thread cache from `settings`, a handle the operator can change at runtime.
    #[must_use]
    pub fn cache(mut self, settings: StaticCacheSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Read files through `fs` instead of the serving thread's compio runtime.
    #[must_use]
    pub fn filesystem<G: FileSystem>(self, fs: G) -> StaticDir<G> {
        StaticDir {
            mount_path: self.mount_path,
            directory: self.directory,
            index_file: self.index_file,
            spa: self.spa,
            cache_control: self.cache_control,
            settings: self.settings,
            fs,
        }
    }
}

impl<F: FileSystem> IntoRouteNode for StaticDir<F> {
    fn into_route_node(self) -> RouteNode {
        let cache = DirCache::new(&self.directory, self.settings);
        let endpoint = StaticDirEndpoint(Rc::new(Shared {
            directory: self.directory,
            index_file: self.index_file,
            spa: self.spa,
            cache_control: self.cache_control,
            fs: self.fs,
            cache,
        }));
        route_node_for_static(self.mount_path, endpoint)
    }
}

/// The endpoint both registrations (the mount point and the wildcard below it) share, with the
/// thread's cache inside.
#[derive(Debug)]
struct StaticDirEndpoint<F>(Rc<Shared<F>>);

impl<F> Clone for StaticDirEndpoint<F> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

#[derive(Debug)]
struct Shared<F> {
    directory: Arc<PathBuf>,
    index_file: String,
    spa: bool,
    cache_control: Option<HeaderValue>,
    fs: F,
    cache: DirCache<Resource>,
}

/// A resolved file: the file itself and whichever precompressed siblings it has.
#[derive(Debug)]
struct Resource {
    content_type: Option<HeaderValue>,
    identity: Representation,
    variants: SmallVec<[(Coding, Representation); 3]>,
}

/// One file's bytes and validators.
#[derive(Debug)]
struct Representation {
    len: u64,
    modified: Option<SystemTime>,
    etag: Option<ETag>,
    content: Content,
}

#[derive(Debug)]
enum Content {
    /// Below the inline limit: the whole file, read once.
    Memory(Bytes),
    /// The open file, sent as a region.
    File(std::fs::File),
}

impl Representation {
    fn body(&self, offset: u64, length: u64) -> io::Result<Body> {
        Ok(match &self.content {
            Content::Memory(bytes) => {
                let range = super::usize_range(offset, length);
                Body::from_bytes(bytes.slice(range))
            }
            // `FileRegion` owns its file, so each response takes a duplicate of the cached
            // descriptor — one `dup`, never an `open`.
            Content::File(file) => {
                Body::from_file_region(http_kit::FileRegion::new(file.try_clone()?, offset, length))
            }
        })
    }
}

impl<F: FileSystem> Shared<F> {
    async fn serve(&self, request: &Request) -> Result<Response, StaticDirError> {
        let relative = requested_path(request)?;
        let key = relative.to_string_lossy();
        let resource = if let Some(resource) = self.cache.get(&key) {
            resource
        } else {
            let generation = self.cache.generation();
            let resource = Rc::new(self.load(&relative).await?);
            self.cache.insert(&key, Rc::clone(&resource), generation);
            resource
        };

        let chosen = encoding::choose(
            request.headers(),
            resource.identity.len,
            resource
                .variants
                .iter()
                .map(|(coding, variant)| (*coding, variant.len)),
        );
        let (coding, representation) = chosen.map_or((None, &resource.identity), |index| {
            let (coding, variant) = &resource.variants[index];
            (Some(*coding), variant)
        });

        let served = ServedFile {
            len: representation.len,
            etag: representation.etag.clone(),
            last_modified: representation.modified,
            content_type: resource.content_type.clone(),
            coding,
            has_variants: !resource.variants.is_empty(),
        };
        let disposition = negotiate(
            request.headers(),
            served.etag.as_ref(),
            served.last_modified,
            served.len,
        );
        let mut response = served.head(&disposition, self.cache_control.as_ref());
        if let Some((offset, length)) = disposition.span(served.len) {
            *response.body_mut() = representation.body(offset, length)?;
        }
        Ok(response)
    }

    /// Resolve `relative` the way the cache stores it: the file, a directory's index, or the SPA
    /// fallback, each with its siblings.
    async fn load(&self, relative: &Path) -> Result<Resource, StaticDirError> {
        let target = if relative.as_os_str().is_empty() {
            self.directory.to_path_buf()
        } else {
            self.directory.join(relative)
        };

        let mut found = match self.open_file(target.clone()).await? {
            Lookup::File(found) => Some(found),
            Lookup::Directory => self
                .open_file(target.join(&self.index_file))
                .await?
                .into_file(),
            Lookup::Missing => None,
        };
        // SPA fallback: only for extensionless paths.
        if found.is_none() && self.spa && relative.extension().is_none() {
            found = self
                .open_file(self.directory.join(&self.index_file))
                .await?
                .into_file();
        }
        let found = found.ok_or(StaticDirError::FileNotFound)?;
        self.resource(found).await.map_err(StaticDirError::IoError)
    }

    /// Open `path` and look for each of its siblings, all in one submission.
    async fn open_file(&self, path: PathBuf) -> io::Result<Lookup<F::Handle>> {
        let siblings = Coding::ALL.map(|coding| {
            let mut sibling = path.clone().into_os_string();
            sibling.push(".");
            sibling.push(coding.extension());
            self.fs.open(PathBuf::from(sibling))
        });
        let [brotli, zstd, gzip] = siblings;
        let (identity, brotli, zstd, gzip) =
            futures_util::future::join4(self.fs.open(path.clone()), brotli, zstd, gzip).await;

        let identity = match absent_is_none(identity)? {
            Some(Entry::File(identity)) => identity,
            Some(Entry::Directory) => return Ok(Lookup::Directory),
            None => return Ok(Lookup::Missing),
        };
        let mut variants = SmallVec::new();
        for (coding, sibling) in Coding::ALL.into_iter().zip([brotli, zstd, gzip]) {
            let Some(Entry::File(sibling)) = absent_is_none(sibling)? else {
                continue;
            };
            if sibling.modified < identity.modified {
                tracing::warn!(
                    file = %path.display(),
                    coding = coding.extension(),
                    "a precompressed sibling is older than its file; serving the file instead \
                     until `skyzen build` regenerates it"
                );
                continue;
            }
            variants.push((coding, sibling));
        }
        Ok(Lookup::File(Found {
            identity: (path, identity),
            variants,
        }))
    }

    /// Turn what was opened into a cache entry, reading every file below the inline limit.
    async fn resource(&self, found: Found<F::Handle>) -> io::Result<Resource> {
        let inline_limit = self.cache.inline_limit();
        let (path, identity) = found.identity;
        let codings: SmallVec<[Coding; 3]> =
            found.variants.iter().map(|(coding, _)| *coding).collect();
        let representations = futures_util::future::try_join_all(
            std::iter::once((None, identity))
                .chain(
                    found
                        .variants
                        .into_iter()
                        .map(|(coding, opened)| (Some(coding), opened)),
                )
                .map(|(coding, opened)| self.representation(coding, opened, inline_limit)),
        )
        .await?;

        let mut representations = representations.into_iter();
        let identity = representations
            .next()
            .expect("the file itself is the first representation");
        Ok(Resource {
            content_type: guess_content_type(&path),
            identity,
            variants: codings.into_iter().zip(representations).collect(),
        })
    }

    async fn representation(
        &self,
        coding: Option<Coding>,
        opened: Opened<F::Handle>,
        inline_limit: u64,
    ) -> io::Result<Representation> {
        let Opened {
            handle,
            len,
            modified,
        } = opened;
        let content = if len < inline_limit {
            Content::Memory(self.fs.read(handle, len).await?)
        } else {
            Content::File(self.fs.std_file(handle)?)
        };
        Ok(Representation {
            len,
            modified,
            etag: metadata_etag(len, modified, coding),
            content,
        })
    }
}

/// What one path turned out to be.
enum Lookup<H> {
    File(Found<H>),
    Directory,
    Missing,
}

impl<H> Lookup<H> {
    /// The file, when the path is one; an index or fallback that is a directory serves nothing.
    fn into_file(self) -> Option<Found<H>> {
        match self {
            Self::File(found) => Some(found),
            Self::Directory | Self::Missing => None,
        }
    }
}

/// A path that resolved to a file, with the siblings found beside it.
struct Found<H> {
    identity: (PathBuf, Opened<H>),
    variants: SmallVec<[(Coding, Opened<H>); 3]>,
}

/// `Ok(None)` for a path that names nothing — missing, or below something that is not a
/// directory — and the error for anything else, which a request must not mistake for a `404`.
fn absent_is_none<H>(result: io::Result<Entry<H>>) -> io::Result<Option<Entry<H>>> {
    match result {
        Ok(opened) => Ok(Some(opened)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// The `ETag` for a filesystem entry: its size and modification time, which change together
/// whenever the content does and cost no read to obtain, plus the coding of a sibling so two
/// representations can never share one.
fn metadata_etag(len: u64, modified: Option<SystemTime>, coding: Option<Coding>) -> Option<ETag> {
    let modified = modified?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    coding
        .map_or_else(
            || format!("\"{len:x}-{modified:x}\""),
            |coding| format!("\"{len:x}-{modified:x}-{}\"", coding.extension()),
        )
        .parse()
        .ok()
}

impl<F: FileSystem> Endpoint for StaticDirEndpoint<F> {
    type Error = StaticDirError;

    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        self.0.serve(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        path::Path,
        time::{Duration, Instant, SystemTime},
    };

    use futures_util::StreamExt as _;
    use http_kit::BodyFrame;

    use super::StaticDir;
    use crate::{
        header::{self, HeaderValue},
        routing::{build, Route, Router},
        static_files::{BlockingFs, StaticCacheSettings},
        Body, Method, Response, StatusCode,
    };

    /// Chinese, Japanese and Korean, so names and bodies that are not ASCII go through intact.
    const CJK: &str = "你好，世界。こんにちは、世界。안녕하세요, 세계.";

    /// How long a test waits for a filesystem notification to reach the cache.
    const NOTIFY_DEADLINE: Duration = Duration::from_secs(10);

    fn run<F: Future>(future: F) -> F::Output {
        crate::runtime::testing::block_on(future)
    }

    fn router(dir: &Path) -> Router {
        build(Route::new((StaticDir::new("/s", dir),))).unwrap()
    }

    fn get_request(path: &str) -> http_kit::Request {
        let mut request = http_kit::Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("invalid path");
        *request.method_mut() = Method::GET;
        request
    }

    fn accepting(path: &str, accept_encoding: &'static str) -> http_kit::Request {
        let mut request = get_request(path);
        request.headers_mut().insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static(accept_encoding),
        );
        request
    }

    fn header(response: &Response, name: header::HeaderName) -> Option<&str> {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().expect("an ASCII header"))
    }

    async fn body(response: Response) -> Vec<u8> {
        response.into_body().into_bytes().await.unwrap().to_vec()
    }

    /// A URI path naming `name`, percent-encoded as a client sends it.
    fn uri(name: &str) -> String {
        use std::fmt::Write as _;

        name.bytes().fold(String::from("/s/"), |mut uri, byte| {
            if byte.is_ascii_alphanumeric() || b"/.-_".contains(&byte) {
                uri.push(char::from(byte));
            } else {
                write!(uri, "%{byte:02X}").expect("writing to a String cannot fail");
            }
            uri
        })
    }

    /// Request `path` until its body is `expected`, for at most [`NOTIFY_DEADLINE`].
    async fn eventually(router: &Router, path: &str, expected: &[u8]) {
        let started = Instant::now();
        loop {
            let response = router.clone().go(get_request(path)).await.unwrap();
            let got = body(response).await;
            if got == expected {
                return;
            }
            assert!(
                started.elapsed() < NOTIFY_DEADLINE,
                "still serving {:?} after {NOTIFY_DEADLINE:?}",
                String::from_utf8_lossy(&got)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn serves_files_from_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("assets");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("main.css"), b"body { color: #fff; }").unwrap();

        run(async {
            let response = router(dir.path())
                .go(get_request("/s/assets/main.css"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(header(&response, header::CONTENT_TYPE), Some("text/css"));
            assert!(response.headers().contains_key(header::ETAG));
            assert!(response.headers().contains_key(header::LAST_MODIFIED));
            assert_eq!(header(&response, header::ACCEPT_RANGES), Some("bytes"));
            assert_eq!(header(&response, header::VARY), None, "no sibling, no Vary");
            assert_eq!(body(response).await, b"body { color: #fff; }");
        });
    }

    #[test]
    fn cjk_names_and_contents_are_served() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("こんにちは");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("你好.txt"), CJK).unwrap();
        std::fs::write(nested.join("index.html"), format!("<p>{CJK}</p>")).unwrap();
        std::fs::write(dir.path().join("안녕.css"), format!("/* {CJK} */")).unwrap();

        run(async {
            let router = router(dir.path());
            for (name, expected) in [
                ("你好.txt", CJK.to_owned()),
                ("こんにちは", format!("<p>{CJK}</p>")),
                ("안녕.css", format!("/* {CJK} */")),
            ] {
                let response = router.clone().go(get_request(&uri(name))).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{name}");
                assert_eq!(body(response).await, expected.as_bytes(), "{name}");
            }
        });
    }

    #[test]
    fn a_matching_etag_answers_304_without_a_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.js"), b"console.log(1)").unwrap();

        run(async {
            let router = router(dir.path());
            let first = router.clone().go(get_request("/s/app.js")).await.unwrap();
            let etag = first.headers().get(header::ETAG).unwrap().clone();

            let mut request = get_request("/s/app.js");
            request.headers_mut().insert(header::IF_NONE_MATCH, etag);
            let response = router.clone().go(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert!(body(response).await.is_empty(), "a 304 carries no body");
        });
    }

    #[test]
    fn a_not_modified_since_request_answers_304() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.js"), b"console.log(1)").unwrap();

        run(async {
            let router = router(dir.path());
            let first = router.clone().go(get_request("/s/app.js")).await.unwrap();
            let modified = first.headers().get(header::LAST_MODIFIED).unwrap().clone();

            let mut request = get_request("/s/app.js");
            request
                .headers_mut()
                .insert(header::IF_MODIFIED_SINCE, modified);
            let response = router.clone().go(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        });
    }

    #[test]
    fn a_single_range_answers_206_with_just_those_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"0123456789").unwrap();

        run(async {
            let mut request = get_request("/s/data.bin");
            request
                .headers_mut()
                .insert(header::RANGE, HeaderValue::from_static("bytes=2-5"));
            let response = router(dir.path()).go(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(
                header(&response, header::CONTENT_RANGE),
                Some("bytes 2-5/10")
            );
            assert_eq!(body(response).await, b"2345");
        });
    }

    /// Above the inline limit a file is sent as a region of the open file, ranges included: the
    /// first frame is the region itself, at the range's offset, which the HTTP/1.1 server hands
    /// to `sendfile`/`splice`.
    #[test]
    fn a_range_of_a_large_file_is_a_file_region() {
        let dir = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0..2_000)
            .flat_map(|line| format!("{line:04} {CJK}\n").into_bytes())
            .collect();
        std::fs::write(dir.path().join("大きい.txt"), &payload).unwrap();
        let settings = StaticCacheSettings::new(16, 1024);
        let router = build(Route::new((
            StaticDir::new("/s", dir.path()).cache(settings),
        )))
        .unwrap();

        run(async {
            for _ in 0..2 {
                // The second pass is served from the cache's open descriptor.
                let mut request = get_request(&uri("大きい.txt"));
                request
                    .headers_mut()
                    .insert(header::RANGE, HeaderValue::from_static("bytes=100-4099"));
                let response = router.clone().go(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
                let expected_range = format!("bytes 100-4099/{}", payload.len());
                assert_eq!(
                    header(&response, header::CONTENT_RANGE),
                    Some(expected_range.as_str())
                );

                let mut body = response.into_body();
                match body.next().await.expect("a frame").expect("no error") {
                    BodyFrame::File(region) => {
                        assert_eq!((region.offset(), region.len()), (100, 4000));
                        let bytes = Body::from_file_region(region).into_bytes().await.unwrap();
                        assert_eq!(&bytes[..], &payload[100..4100]);
                    }
                    other => panic!("expected a file region, got {other:?}"),
                }
            }
        });
    }

    #[test]
    fn a_small_file_is_held_in_memory_until_the_limit_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("小.txt"), CJK).unwrap();
        let settings = StaticCacheSettings::new(16, 64 * 1024);
        let router = build(Route::new((
            StaticDir::new("/s", dir.path()).cache(settings.clone()),
        )))
        .unwrap();

        run(async {
            let response = router
                .clone()
                .go(get_request(&uri("小.txt")))
                .await
                .unwrap();
            assert!(
                response.body().try_clone().is_some(),
                "below the limit the body is in-memory bytes"
            );

            // The operator lowers the limit while the server runs: the next lookup rebuilds the
            // cache, and the same file now goes out as a region.
            settings.set_inline_limit(0);
            let response = router
                .clone()
                .go(get_request(&uri("小.txt")))
                .await
                .unwrap();
            let mut body = response.into_body();
            let frame = body.next().await.expect("a frame").expect("no error");
            assert!(frame.as_file().is_some(), "got {frame:?}");
        });
    }

    #[test]
    fn a_range_past_the_end_answers_416() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"0123456789").unwrap();

        run(async {
            let mut request = get_request("/s/data.bin");
            request
                .headers_mut()
                .insert(header::RANGE, HeaderValue::from_static("bytes=50-60"));
            let response = router(dir.path()).go(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(header(&response, header::CONTENT_RANGE), Some("bytes */10"));
        });
    }

    #[test]
    fn a_configured_cache_control_reaches_the_response() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.js"), b"console.log(1)").unwrap();
        let router = build(Route::new((StaticDir::new("/s", dir.path())
            .cache_control(HeaderValue::from_static("public, max-age=60")),)))
        .unwrap();

        run(async {
            let response = router.go(get_request("/s/app.js")).await.unwrap();
            assert_eq!(
                header(&response, header::CACHE_CONTROL),
                Some("public, max-age=60")
            );
        });
    }

    /// The siblings `skyzen build` writes, each with distinct bytes so the test can tell which
    /// was sent: brotli smallest, then zstd, then gzip.
    fn with_variants(dir: &Path) {
        std::fs::write(
            dir.join("app.js"),
            format!("console.log({CJK:?}); // identity"),
        )
        .unwrap();
        std::fs::write(dir.join("app.js.br"), "br").unwrap();
        std::fs::write(dir.join("app.js.zst"), "zstd").unwrap();
        std::fs::write(dir.join("app.js.gz"), "gzip!").unwrap();
    }

    #[test]
    fn accept_encoding_picks_the_variant() {
        let dir = tempfile::tempdir().unwrap();
        with_variants(dir.path());

        run(async {
            let router = router(dir.path());
            for (accept, coding, expected) in [
                ("gzip, deflate, br, zstd", Some("br"), &b"br"[..]),
                ("gzip", Some("gzip"), b"gzip!"),
                ("x-gzip", Some("gzip"), b"gzip!"),
                ("br;q=0, zstd;q=0.5, gzip;q=0.4", Some("zstd"), b"zstd"),
                ("*;q=0.1, identity", None, b""),
                ("deflate", None, b""),
            ] {
                let response = router
                    .clone()
                    .go(accepting("/s/app.js", accept))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{accept}");
                assert_eq!(
                    header(&response, header::CONTENT_ENCODING),
                    coding,
                    "{accept}"
                );
                assert_eq!(
                    header(&response, header::VARY),
                    Some("accept-encoding"),
                    "{accept}"
                );
                assert_eq!(
                    header(&response, header::CONTENT_TYPE),
                    Some("text/javascript"),
                    "the variant keeps the file's type: {accept}"
                );
                let got = body(response).await;
                if coding.is_some() {
                    assert_eq!(got, expected, "{accept}");
                } else {
                    assert!(got.ends_with(b"// identity"), "{accept}");
                }
            }
        });
    }

    #[test]
    fn without_accept_encoding_the_file_itself_is_sent_with_vary() {
        let dir = tempfile::tempdir().unwrap();
        with_variants(dir.path());

        run(async {
            let router = router(dir.path());
            let plain = router.clone().go(get_request("/s/app.js")).await.unwrap();
            assert_eq!(header(&plain, header::CONTENT_ENCODING), None);
            assert_eq!(header(&plain, header::VARY), Some("accept-encoding"));
            let plain_etag = header(&plain, header::ETAG).unwrap().to_owned();

            let brotli = router
                .clone()
                .go(accepting("/s/app.js", "br"))
                .await
                .unwrap();
            assert_ne!(
                header(&brotli, header::ETAG),
                Some(plain_etag.as_str()),
                "each representation validates on its own"
            );

            // A 304 for the variant still says what it varies on.
            let etag = brotli.headers().get(header::ETAG).unwrap().clone();
            let mut request = accepting("/s/app.js", "br");
            request.headers_mut().insert(header::IF_NONE_MATCH, etag);
            let response = router.clone().go(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(header(&response, header::VARY), Some("accept-encoding"));
        });
    }

    #[test]
    fn a_range_applies_to_the_chosen_variant() {
        let dir = tempfile::tempdir().unwrap();
        with_variants(dir.path());

        run(async {
            let mut request = accepting("/s/app.js", "gzip");
            request
                .headers_mut()
                .insert(header::RANGE, HeaderValue::from_static("bytes=1-3"));
            let response = router(dir.path()).go(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(header(&response, header::CONTENT_ENCODING), Some("gzip"));
            assert_eq!(
                header(&response, header::CONTENT_RANGE),
                Some("bytes 1-3/5")
            );
            assert_eq!(body(response).await, b"zip");
        });
    }

    #[test]
    fn a_sibling_older_than_its_file_is_not_served() {
        let dir = tempfile::tempdir().unwrap();
        with_variants(dir.path());
        let long_ago = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        std::fs::File::options()
            .write(true)
            .open(dir.path().join("app.js.br"))
            .unwrap()
            .set_modified(long_ago)
            .unwrap();

        run(async {
            let response = router(dir.path())
                .go(accepting("/s/app.js", "br, zstd"))
                .await
                .unwrap();
            assert_eq!(
                header(&response, header::CONTENT_ENCODING),
                Some("zstd"),
                "the stale brotli sibling is skipped"
            );
        });
    }

    #[test]
    fn a_rewritten_file_is_served_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("数据.txt");
        std::fs::write(&path, "第一版").unwrap();

        run(async {
            let router = router(dir.path());
            let first = router
                .clone()
                .go(get_request(&uri("数据.txt")))
                .await
                .unwrap();
            assert_eq!(body(first).await, "第一版".as_bytes());

            std::fs::write(&path, format!("第二版 — {CJK}")).unwrap();
            eventually(
                &router,
                &uri("数据.txt"),
                format!("第二版 — {CJK}").as_bytes(),
            )
            .await;
        });
    }

    #[test]
    fn a_file_replaced_by_rename_is_served_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("번들.js");
        std::fs::write(&path, "옛날").unwrap();
        let settings = StaticCacheSettings::new(16, 0);
        let router = build(Route::new((
            StaticDir::new("/s", dir.path()).cache(settings),
        )))
        .unwrap();

        run(async {
            let first = router
                .clone()
                .go(get_request(&uri("번들.js")))
                .await
                .unwrap();
            assert_eq!(body(first).await, "옛날".as_bytes());

            // The atomic deploy: write beside, rename over. The cached descriptor still reads the
            // old inode, so only the notification can make the new one visible.
            let staged = dir.path().join("번들.js.tmp");
            std::fs::write(&staged, format!("새로운 {CJK}")).unwrap();
            std::fs::rename(&staged, &path).unwrap();
            eventually(&router, &uri("번들.js"), format!("새로운 {CJK}").as_bytes()).await;
        });
    }

    #[test]
    fn a_new_index_file_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs").join("index.html"), "旧").unwrap();

        run(async {
            let router = router(dir.path());
            eventually(&router, "/s/docs", "旧".as_bytes()).await;
            std::fs::remove_file(dir.path().join("docs").join("index.html")).unwrap();
            std::fs::write(dir.path().join("docs").join("index.html"), "新しい").unwrap();
            eventually(&router, "/s/docs", "新しい".as_bytes()).await;
        });
    }

    #[test]
    fn serves_index_file_for_root_requests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>Home</h1>").unwrap();

        run(async {
            let router = build(Route::new((StaticDir::new("/public", dir.path()),))).unwrap();
            let response = router.go(get_request("/public")).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body(response).await, b"<h1>Home</h1>");
        });
    }

    #[test]
    fn a_root_mount_serves_its_index_at_slash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), CJK).unwrap();

        run(async {
            let router = build(Route::new((StaticDir::new("/", dir.path()),))).unwrap();
            let response = router.go(get_request("/")).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body(response).await, CJK.as_bytes());
        });
    }

    #[test]
    fn blocks_path_traversal_attempts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"root").unwrap();

        run(async {
            let error = router(dir.path())
                .go(get_request("/s/../Cargo.toml"))
                .await
                .unwrap_err();
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        });
    }

    #[test]
    fn returns_not_found_for_missing_files() {
        let dir = tempfile::tempdir().unwrap();

        run(async {
            let error = router(dir.path())
                .go(get_request("/s/app.js"))
                .await
                .unwrap_err();
            assert_eq!(error.status(), StatusCode::NOT_FOUND);
            let error = router(dir.path())
                .go(get_request("/s/app.js/below-a-file"))
                .await
                .unwrap_err();
            assert_eq!(error.status(), StatusCode::NOT_FOUND);
        });
    }

    #[test]
    fn honors_custom_index_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("home.htm"), b"custom").unwrap();

        run(async {
            let router = build(Route::new((
                StaticDir::new("/web", dir.path()).index_file("home.htm"),
            )))
            .unwrap();
            let response = router.go(get_request("/web")).await.unwrap();
            assert_eq!(body(response).await, b"custom");
        });
    }

    #[test]
    fn spa_fallback_serves_index_for_extensionless_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>SPA</h1>").unwrap();

        run(async {
            let router = build(Route::new((StaticDir::new("/app", dir.path()).spa(),))).unwrap();
            let response = router.clone().go(get_request("/app/about")).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body(response).await, b"<h1>SPA</h1>");

            let error = router.go(get_request("/app/missing.js")).await.unwrap_err();
            assert_eq!(error.status(), StatusCode::NOT_FOUND);
        });
    }

    #[test]
    fn non_spa_returns_404_for_extensionless_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>Home</h1>").unwrap();

        run(async {
            let error = router(dir.path())
                .go(get_request("/s/about"))
                .await
                .unwrap_err();
            assert_eq!(error.status(), StatusCode::NOT_FOUND);
        });
    }

    /// A router served by Tokio reads through the blocking pool instead of compio.
    #[tokio::test]
    async fn blocking_fs_serves_outside_compio() {
        let dir = tempfile::tempdir().unwrap();
        with_variants(dir.path());
        let router = build(Route::new((
            StaticDir::new("/s", dir.path()).filesystem(BlockingFs),
        )))
        .unwrap();

        let response = router.go(accepting("/s/app.js", "zstd")).await.unwrap();
        assert_eq!(header(&response, header::CONTENT_ENCODING), Some("zstd"));
        assert_eq!(body(response).await, b"zstd");
    }

    #[test]
    #[should_panic(expected = "cannot watch")]
    fn a_directory_that_cannot_be_watched_stops_the_router() {
        let dir = tempfile::tempdir().unwrap();
        let _ = router(&dir.path().join("does-not-exist"));
    }
}
