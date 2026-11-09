//! HTTP cookies
pub use cookie::{Cookie, Key};
use core::future::{ready, Future};
use http::StatusCode;

use std::{
    any::TypeId,
    ops::{Deref, DerefMut},
    str::FromStr,
    sync::OnceLock,
};

use futures_core::future::BoxFuture;
use http_kit::{
    header::{self, HeaderValue},
    http_error, HttpError, Request, Response,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor, Requirement, Responder, Secret,
};

/// A collection of cookies that tracks its modifications.
#[derive(Debug)]
pub struct CookieJar(cookie::CookieJar);

impl Deref for CookieJar {
    type Target = cookie::CookieJar;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for CookieJar {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl FromStr for CookieJar {
    type Err = cookie::ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let cookies = cookie::Cookie::split_parse_encoded(s);
        let mut jar = cookie::CookieJar::new();
        for cookie in cookies {
            let cookie = cookie?.into_owned();
            jar.add_original(cookie);
        }
        Ok(Self(jar))
    }
}

http_error!(
    /// Error occurs when parsing cookies from request headers.
    pub CookieParseError, StatusCode::BAD_REQUEST, "Failed to parse cookies"
);

impl Extractor for CookieJar {
    type Error = CookieParseError;
    // The header is already on the request, so the future is ready on creation rather than an
    // `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(parse_jar(request))
    }
}

/// Join every `Cookie` field line and parse the result.
///
/// HTTP/2 allows the header to be split into several field lines, so they are joined with "; "
/// before parsing.
fn parse_jar(request: &Request) -> Result<CookieJar, CookieParseError> {
    let mut combined = String::new();
    for value in request.headers().get_all(header::COOKIE) {
        let value = core::str::from_utf8(value.as_bytes()).map_err(|_| CookieParseError::new())?;
        if !combined.is_empty() {
            combined.push_str("; ");
        }
        combined.push_str(value);
    }
    combined
        .parse::<CookieJar>()
        .map_err(|_| CookieParseError::new())
}

http_error!(
    /// Error occurs when setting cookies to response headers.
    pub CookieSetError, StatusCode::INTERNAL_SERVER_ERROR, "Failed to set cookies"
);

/// The signing and encryption keys [`SignedCookieJar`] and [`PrivateCookieJar`] work against.
///
/// Stored in the request extensions by the layer [`CookieKeys::new`] builds; extracting a
/// protected jar without it fails at route build.
#[derive(Debug, Clone)]
pub struct CookieKeys {
    primary: Key,
    fallback: Option<Key>,
    /// Whether cookies written through the jars default to `Secure`. On for the real thing,
    /// off for `http://localhost` development where a `Secure` cookie never reaches the jar.
    secure: bool,
}

impl CookieKeys {
    /// Build a layer carrying a ready-made key — [`Key::generate`] for a quick start, a key
    /// loaded from elsewhere otherwise.
    ///
    /// `new` deliberately returns the layer, not `Self`: `CookieKeys` is the extension type,
    /// and `CookieKeys::new(..)` building the layer keeps both roles under one name.
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub const fn new(key: Key) -> CookieKeysLayer {
        CookieKeysLayer::from_ready(key)
    }

    /// Build a layer whose key is derived from the manifest secret `S` — the generated type
    /// `import_config!` produces for a `[[secret]]` entry — read from the request extensions.
    /// The master secret must be at least 32 bytes; the key itself is HKDF-expanded.
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn from_secret<S>() -> CookieKeysLayer
    where
        S: Extractor + Deref<Target = Secret> + 'static,
        S::Error: HttpError,
    {
        CookieKeysLayer::from_secret::<S>()
    }

    /// The key writes are signed or encrypted with.
    #[must_use]
    pub const fn key(&self) -> &Key {
        &self.primary
    }

    /// The previous key still accepted for verification, when rotation is configured.
    #[must_use]
    pub const fn fallback(&self) -> Option<&Key> {
        self.fallback.as_ref()
    }

    /// Whether the jars default `Secure` on.
    #[must_use]
    pub const fn secure(&self) -> bool {
        self.secure
    }
}

/// How a layer resolves one [`Key`] — a supplied one, or a manifest secret read once and
/// HKDF-expanded on the first request that needs it.
enum KeySource {
    Ready(Key),
    Secret {
        name: &'static str,
        resolve: fn(&mut Request) -> BoxFuture<'_, Result<Key, CookieKeysError>>,
    },
}

impl core::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Ready(_) => f.write_str("Ready(..)"),
            Self::Secret { name, .. } => f.debug_tuple("Secret").field(name).finish(),
        }
    }
}

/// The layer [`CookieKeys::new`] and [`CookieKeys::from_secret`] build.
///
/// Resolves its key (and an optional rotation fallback) on the first request, caches the
/// expanded `Key` for the rest of the process and inserts [`CookieKeys`] into the request
/// extensions for the jars to read.
#[derive(Debug)]
pub struct CookieKeysLayer {
    primary: KeySource,
    fallback: Option<KeySource>,
    secure: bool,
    resolved: OnceLock<Result<CookieKeys, CookieKeysError>>,
}

impl CookieKeysLayer {
    const fn from_ready(key: Key) -> Self {
        Self {
            primary: KeySource::Ready(key),
            fallback: None,
            secure: true,
            resolved: OnceLock::new(),
        }
    }

    fn from_secret<S>() -> Self
    where
        S: Extractor + Deref<Target = Secret> + 'static,
        S::Error: HttpError,
    {
        Self {
            primary: KeySource::Secret {
                name: core::any::type_name::<S>(),
                resolve: |request| Box::pin(async move { key_from_secret::<S>(request).await }),
            },
            fallback: None,
            secure: true,
            resolved: OnceLock::new(),
        }
    }

    /// Keep verifying cookies signed or encrypted by `S` — the previous secret during rotation —
    /// while writing with the primary key.
    #[must_use]
    pub fn with_fallback<S>(mut self) -> Self
    where
        S: Extractor + Deref<Target = Secret> + 'static,
        S::Error: HttpError,
    {
        self.fallback = Some(KeySource::Secret {
            name: core::any::type_name::<S>(),
            resolve: |request| Box::pin(async move { key_from_secret::<S>(request).await }),
        });
        self
    }

    /// Keep verifying with a ready-made previous key during rotation.
    #[must_use]
    pub const fn with_fallback_key(mut self, key: Key) -> Self {
        self.fallback = Some(KeySource::Ready(key));
        self
    }

    /// Drop `Secure` from the cookie defaults — for `http://localhost` development only.
    #[must_use]
    pub const fn secure_cookies(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    /// Resolve both keys, once, and cache the result for the process.
    async fn resolve(&self, request: &mut Request) -> Result<&CookieKeys, CookieKeysError> {
        if let Some(resolved) = self.resolved.get() {
            return resolved.as_ref().map_err(CookieKeysError::clone);
        }
        let resolved = self.resolve_uncached(request).await;
        // A resolution failure is not cached: a secret that arrives late (a provider that only
        // populates extensions on later requests) must be allowed to recover.
        match resolved {
            Ok(keys) => Ok(self.resolved.get_or_init(|| Ok(keys)).as_ref().unwrap()),
            Err(error) => Err(error),
        }
    }

    async fn resolve_uncached(&self, request: &mut Request) -> Result<CookieKeys, CookieKeysError> {
        let primary = resolve_key(&self.primary, request).await?;
        let fallback = match &self.fallback {
            Some(source) => Some(resolve_key(source, request).await?),
            None => None,
        };
        Ok(CookieKeys {
            primary,
            fallback,
            secure: self.secure,
        })
    }
}

async fn resolve_key(source: &KeySource, request: &mut Request) -> Result<Key, CookieKeysError> {
    match source {
        KeySource::Ready(key) => Ok(key.clone()),
        KeySource::Secret { resolve, .. } => resolve(request).await,
    }
}

/// HKDF-expand a manifest secret `S` into a cookie `Key`, enforcing the 32-byte floor.
async fn key_from_secret<S>(request: &mut Request) -> Result<Key, CookieKeysError>
where
    S: Extractor + Deref<Target = Secret> + 'static,
    S::Error: HttpError,
{
    let name = core::any::type_name::<S>();
    let secret = S::extract(request).await.map_err(|error| CookieKeysError {
        kind: CookieKeysErrorKind::MissingSecret {
            name,
            source: Box::new(error),
        },
    })?;
    let bytes = secret.expose().as_bytes();
    if bytes.len() < 32 {
        return Err(CookieKeysError {
            kind: CookieKeysErrorKind::ShortSecret {
                name,
                length: bytes.len(),
            },
        });
    }
    Ok(Key::derive_from(bytes))
}

impl Middleware for CookieKeysLayer {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let keys = self.resolve(request).await.map_err(Error::from)?;
        request.extensions_mut().insert(keys.clone());
        next.run(request).await
    }

    fn provisions(&self) -> Vec<TypeId> {
        vec![TypeId::of::<CookieKeys>()]
    }
}

/// Why a [`CookieKeys`] layer could not produce keys.
#[derive(Debug)]
pub struct CookieKeysError {
    kind: CookieKeysErrorKind,
}

impl Clone for CookieKeysError {
    fn clone(&self) -> Self {
        match &self.kind {
            CookieKeysErrorKind::MissingSecret { name, source } => Self {
                kind: CookieKeysErrorKind::MissingSecret {
                    name,
                    source: source.to_string().into(),
                },
            },
            CookieKeysErrorKind::ShortSecret { name, length } => Self {
                kind: CookieKeysErrorKind::ShortSecret {
                    name,
                    length: *length,
                },
            },
        }
    }
}

#[derive(Debug)]
enum CookieKeysErrorKind {
    MissingSecret {
        name: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    ShortSecret {
        name: &'static str,
        length: usize,
    },
}

impl core::fmt::Display for CookieKeysError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.kind {
            CookieKeysErrorKind::MissingSecret { name, .. } => {
                write!(f, "cookie key secret `{name}` is not on the request")
            }
            CookieKeysErrorKind::ShortSecret { name, length } => {
                write!(
                    f,
                    "cookie key secret `{name}` is {length} bytes; at least 32 are required"
                )
            }
        }
    }
}

impl std::error::Error for CookieKeysError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            CookieKeysErrorKind::MissingSecret { source, .. } => Some(&**source),
            CookieKeysErrorKind::ShortSecret { .. } => None,
        }
    }
}

impl HttpError for CookieKeysError {
    fn status(&self) -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// The secure defaults a jar applies to a cookie written through `add`, unless the caller set
/// them already.
fn apply_defaults(cookie: &mut Cookie<'_>, secure: bool) {
    if cookie.path().is_none() {
        cookie.set_path("/");
    }
    if cookie.http_only().is_none() {
        cookie.set_http_only(true);
    }
    if cookie.same_site().is_none() {
        cookie.set_same_site(cookie::SameSite::Lax);
    }
    if cookie.secure().is_none() {
        cookie.set_secure(secure);
    }
}

/// Parse the request's `Cookie` headers into a raw jar, for the session extractor.
pub(crate) fn parse_request_jar(request: &Request) -> Result<cookie::CookieJar, CookieParseError> {
    parse_jar(request).map(|jar| jar.0)
}

fn request_keys(request: &Request) -> Result<CookieKeys, MissingCookieKeys> {
    request
        .extensions()
        .get::<CookieKeys>()
        .cloned()
        .ok_or_else(MissingCookieKeys::new)
}

http_error!(
    /// A protected jar was extracted without a [`CookieKeys`] layer.
    pub MissingCookieKeys,
    StatusCode::INTERNAL_SERVER_ERROR,
    "no `CookieKeys` in the request extensions; add `.layer(CookieKeys::from_secret::<..>())`"
);

/// A cookie jar whose reads verify signatures and whose writes are signed.
///
/// Extracted as `jar: SignedCookieJar` once a [`CookieKeys`] layer is on the route. `get` accepts
/// a cookie verified by the fallback key and re-signs it with the primary one, so a rotation
/// transparently upgrades the first cookie a client presents.
#[derive(Debug)]
pub struct SignedCookieJar {
    jar: cookie::CookieJar,
    keys: CookieKeys,
}

impl SignedCookieJar {
    /// The verified cookie `name`, or `None` when it is absent or its signature fails.
    /// A cookie verified by the fallback key is re-signed with the primary key before it is
    /// handed out.
    pub fn get(&mut self, name: &str) -> Option<Cookie<'static>> {
        if let Some(cookie) = self.jar.signed(&self.keys.primary).get(name) {
            return Some(cookie);
        }
        let cookie = self
            .keys
            .fallback
            .as_ref()
            .and_then(|fallback| self.jar.signed(fallback).get(name))?;
        self.jar.signed_mut(&self.keys.primary).add(cookie.clone());
        Some(cookie)
    }

    /// Sign `cookie` and queue it for the response. `Path=/`, `HttpOnly`, `SameSite=Lax` and
    /// `Secure` are filled in when unset.
    pub fn add(&mut self, mut cookie: Cookie<'static>) {
        apply_defaults(&mut cookie, self.keys.secure);
        self.jar.signed_mut(&self.keys.primary).add(cookie);
    }

    /// Sign `cookie` as expired so the client drops it.
    pub fn remove(&mut self, mut cookie: Cookie<'static>) {
        apply_defaults(&mut cookie, self.keys.secure);
        self.jar.signed_mut(&self.keys.primary).remove(cookie);
    }
}

/// Build a protected jar from the request: parse the header, then read the resolved keys.
fn protected_jar(request: &Request) -> Result<(cookie::CookieJar, CookieKeys), CookieJarError> {
    Ok((parse_request_jar(request)?, request_keys(request)?))
}

impl Extractor for SignedCookieJar {
    type Error = CookieJarError;
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(protected_jar(request).map(|(jar, keys)| Self { jar, keys }))
    }

    fn requirements() -> Vec<Requirement> {
        vec![Requirement::of::<CookieKeys>(
            "`.layer(CookieKeys::from_secret::<..>())`",
        )]
    }
}

impl Responder for SignedCookieJar {
    type Error = CookieSetError;
    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        CookieJar(self.jar).respond_to(request, response)
    }
}

/// A cookie jar whose reads and writes are encrypted and authenticated.
///
/// Same contract as [`SignedCookieJar`], but `get`/`add`/`remove` run through the cookie crate's
/// AES-GCM private jar, so the values are opaque to the client.
#[derive(Debug)]
pub struct PrivateCookieJar {
    jar: cookie::CookieJar,
    keys: CookieKeys,
}

impl PrivateCookieJar {
    /// The decrypted cookie `name`, or `None` when absent or undecryptable. A cookie decrypted by
    /// the fallback key is re-encrypted with the primary key.
    pub fn get(&mut self, name: &str) -> Option<Cookie<'static>> {
        if let Some(cookie) = self.jar.private(&self.keys.primary).get(name) {
            return Some(cookie);
        }
        let cookie = self
            .keys
            .fallback
            .as_ref()
            .and_then(|fallback| self.jar.private(fallback).get(name))?;
        self.jar.private_mut(&self.keys.primary).add(cookie.clone());
        Some(cookie)
    }

    /// Encrypt `cookie` and queue it for the response. `Path=/`, `HttpOnly`, `SameSite=Lax` and
    /// `Secure` are filled in when unset.
    pub fn add(&mut self, mut cookie: Cookie<'static>) {
        apply_defaults(&mut cookie, self.keys.secure);
        self.jar.private_mut(&self.keys.primary).add(cookie);
    }

    /// Encrypt `cookie` as expired so the client drops it.
    pub fn remove(&mut self, mut cookie: Cookie<'static>) {
        apply_defaults(&mut cookie, self.keys.secure);
        self.jar.private_mut(&self.keys.primary).remove(cookie);
    }
}

impl Extractor for PrivateCookieJar {
    type Error = CookieJarError;
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(protected_jar(request).map(|(jar, keys)| Self { jar, keys }))
    }

    fn requirements() -> Vec<Requirement> {
        vec![Requirement::of::<CookieKeys>(
            "`.layer(CookieKeys::from_secret::<..>())`",
        )]
    }
}

impl Responder for PrivateCookieJar {
    type Error = CookieSetError;
    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        CookieJar(self.jar).respond_to(request, response)
    }
}

/// Why a protected jar could not be extracted.
#[skyzen::error(status = StatusCode::BAD_REQUEST)]
pub enum CookieJarError {
    /// The `Cookie` header could not be parsed.
    #[error("Failed to parse cookies")]
    Parse(#[from] CookieParseError),
    /// No [`CookieKeys`] layer provided the keys.
    #[error(
        "no `CookieKeys` in the request extensions; add `.layer(CookieKeys::from_secret::<..>())`",
        status = StatusCode::INTERNAL_SERVER_ERROR
    )]
    MissingKeys(#[from] MissingCookieKeys),
}

impl Responder for CookieJar {
    type Error = CookieSetError;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        for cookie in self.0.delta() {
            response.headers_mut().append(
                header::SET_COOKIE,
                HeaderValue::try_from(cookie.encoded().to_string())
                    .map_err(|_| CookieSetError::new())?,
            ); // TODO: reduce unnecessary header value check
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use http_kit::{
        header::{HeaderValue, COOKIE, SET_COOKIE},
        HttpError,
    };
    use skyzen_core::{Extractor, Responder, Secret};

    use super::{
        Cookie, CookieJar, CookieJarError, CookieKeys, Key, PrivateCookieJar, SignedCookieJar,
    };
    use crate::{
        routing::{CreateRouteNode, Route},
        Body, Request, Response, StatusCode,
    };
    use std::ops::Deref;

    #[tokio::test]
    async fn extracts_multiple_percent_encoded_cookies() {
        let mut request = Request::new(Body::empty());
        request.headers_mut().insert(
            COOKIE,
            HeaderValue::from_static("session=abc%20123; theme=dark"),
        );

        let jar = CookieJar::extract(&mut request).await.unwrap();

        assert_eq!(jar.get("session").unwrap().value(), "abc 123");
        assert_eq!(jar.get("theme").unwrap().value(), "dark");
    }

    #[tokio::test]
    async fn joins_multiple_cookie_headers() {
        let mut request = Request::new(Body::empty());
        request
            .headers_mut()
            .append(COOKIE, HeaderValue::from_static("session=abc"));
        request
            .headers_mut()
            .append(COOKIE, HeaderValue::from_static("theme=dark"));

        let jar = CookieJar::extract(&mut request).await.unwrap();

        assert_eq!(jar.get("session").unwrap().value(), "abc");
        assert_eq!(jar.get("theme").unwrap().value(), "dark");
    }

    #[tokio::test]
    async fn rejects_invalid_utf8_cookie_headers() {
        let mut request = Request::new(Body::empty());
        request
            .headers_mut()
            .insert(COOKIE, HeaderValue::from_bytes(b"\xFF").unwrap());

        let error = CookieJar::extract(&mut request).await.unwrap_err();

        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn responder_emits_only_cookie_delta_headers() {
        let request = Request::new(Body::empty());
        let mut response = Response::new(Body::empty());
        let mut jar: CookieJar = "session=old".parse().unwrap();
        jar.add(Cookie::new("theme", "dark"));

        jar.respond_to(&request, &mut response).unwrap();

        let set_cookies: Vec<_> = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(set_cookies.len(), 1);
        assert!(set_cookies[0].starts_with("theme=dark"));
        assert!(set_cookies
            .iter()
            .all(|value| !value.starts_with("session=old")));
    }

    /// A manifest-shaped secret the tests can put in request extensions.
    #[derive(Clone)]
    struct TestSecret(Secret);

    impl Deref for TestSecret {
        type Target = Secret;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Extractor for TestSecret {
        type Error = CookieJarError;
        fn extract(
            request: &mut Request,
        ) -> impl core::future::Future<Output = Result<Self, Self::Error>> {
            std::future::ready(Ok(request
                .extensions()
                .get::<Self>()
                .cloned()
                .expect("TestSecret injected")))
        }
    }

    fn keys_layer() -> super::CookieKeysLayer {
        CookieKeys::new(Key::generate())
    }

    async fn set_signed(mut jar: SignedCookieJar) -> SignedCookieJar {
        jar.add(Cookie::new("session", "abc-123"));
        jar
    }

    async fn get_signed(mut jar: SignedCookieJar) -> String {
        jar.get("session")
            .map(|cookie| cookie.value().to_owned())
            .unwrap_or_default()
    }

    fn request_with_cookies(cookies: &str) -> Request {
        let mut request = Request::new(Body::empty());
        request
            .headers_mut()
            .insert(COOKIE, HeaderValue::from_bytes(cookies.as_bytes()).unwrap());
        request
    }

    fn set_cookie_of(response: &Response) -> String {
        response
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn a_signed_cookie_round_trips_and_tampering_fails() {
        let router = Route::new(("/set".at(set_signed), "/get".at(get_signed)))
            .layer(keys_layer())
            .build();

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/set".parse().unwrap();
        let response = router.go(request).await.unwrap();
        let set_cookie = set_cookie_of(&response);
        let cookie_pair = set_cookie.split(';').next().unwrap().to_owned();
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"));
        assert!(set_cookie.contains("Secure"));

        let mut request = request_with_cookies(&cookie_pair);
        *request.uri_mut() = "/get".parse().unwrap();
        let response = router.go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "abc-123");

        // A tampered value no longer verifies.
        let mut request = request_with_cookies("session=abc-124%2Bfakesig%3D");
        *request.uri_mut() = "/get".parse().unwrap();
        let response = router.go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "");
    }

    async fn set_private(mut jar: PrivateCookieJar) -> PrivateCookieJar {
        jar.add(Cookie::new("session", "opaque-value"));
        jar
    }

    async fn get_private(mut jar: PrivateCookieJar) -> String {
        jar.get("session")
            .map(|cookie| cookie.value().to_owned())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_private_cookie_is_opaque_and_round_trips() {
        let router = Route::new(("/set".at(set_private), "/get".at(get_private)))
            .layer(keys_layer())
            .build();

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/set".parse().unwrap();
        let response = router.go(request).await.unwrap();
        let cookie_pair = set_cookie_of(&response)
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert!(!cookie_pair.contains("opaque-value"));

        let mut request = request_with_cookies(&cookie_pair);
        *request.uri_mut() = "/get".parse().unwrap();
        let response = router.go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "opaque-value");
    }

    async fn get_and_return_jar(mut jar: SignedCookieJar) -> (SignedCookieJar, String) {
        let value = jar
            .get("session")
            .map(|cookie| cookie.value().to_owned())
            .unwrap_or_default();
        (jar, value)
    }

    #[tokio::test]
    async fn the_fallback_key_verifies_and_the_primary_resigns() {
        let old = Key::generate();
        let new = Key::generate();
        let router = Route::new(("/get".at(get_and_return_jar),))
            .layer(CookieKeys::new(new.clone()).with_fallback_key(old.clone()))
            .build();

        // A cookie the old key signed.
        let mut jar = cookie::CookieJar::new();
        jar.signed_mut(&old).add(Cookie::new("session", "rotating"));
        let header = jar.iter().next().unwrap().encoded().to_string();

        let mut request = request_with_cookies(&header);
        *request.uri_mut() = "/get".parse().unwrap();
        let response = router.go(request).await.unwrap();
        // The response carried a re-signed cookie...
        assert!(response.headers().contains_key(SET_COOKIE));
        let resigned = set_cookie_of(&response)
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let body = response.into_body().into_string().await.unwrap();
        // ...signed by the primary key this time, which the same jar reads back.
        assert_eq!(body, "rotating");
        let mut jar = cookie::CookieJar::new();
        for pair in resigned.split("; ") {
            if let Ok(cookie) = cookie::Cookie::parse_encoded(pair.to_owned()) {
                jar.add_original(cookie.into_owned());
            }
        }
        assert!(jar.signed(&new).get("session").is_some());
    }

    #[tokio::test]
    async fn a_short_secret_is_rejected() {
        let router = Route::new(("/get".at(get_signed),))
            .layer(CookieKeys::from_secret::<TestSecret>())
            .build();
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/get".parse().unwrap();
        request
            .extensions_mut()
            .insert(TestSecret(Secret::new("too short")));
        let error = router.go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(error.to_string().contains("32"));
    }

    #[tokio::test]
    async fn a_manifest_secret_drives_the_keys() {
        let router = Route::new(("/set".at(set_signed), "/get".at(get_signed)))
            .layer(CookieKeys::from_secret::<TestSecret>())
            .build();
        let secret = TestSecret(Secret::new("a master secret well over 32 bytes long"));

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/set".parse().unwrap();
        request.extensions_mut().insert(secret.clone());
        let response = router.go(request).await.unwrap();
        let cookie_pair = set_cookie_of(&response)
            .split(';')
            .next()
            .unwrap()
            .to_owned();

        let mut request = request_with_cookies(&cookie_pair);
        *request.uri_mut() = "/get".parse().unwrap();
        request.extensions_mut().insert(secret);
        let response = router.go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "abc-123");
    }

    #[test]
    fn extracting_a_signed_jar_without_keys_fails_at_build() {
        let error = Route::new(("/get".at(get_signed),))
            .try_build()
            .unwrap_err();
        assert!(matches!(
            error,
            crate::routing::RouteBuildError::MissingProvision { .. }
        ));
    }
}
