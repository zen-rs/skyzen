//! `Csrf` — cross-site request forgery protection for cookie-authenticated routes.
//!
//! Defence in depth for unsafe methods:
//!
//! 1. `Sec-Fetch-Site` is honoured first — modern browsers always send it, and a
//!    `cross-site` value rejects outright.
//! 2. When it is absent (older browsers, some non-browser clients), `Origin` must match the
//!    allow-list; `Referer` is the fallback source.
//! 3. Optionally, `.allow_double_submit(cookie, header)` admits clients that echo a
//!    per-session cookie value back in a header — the legacy double-submit pattern for
//!    clients that cannot send `Sec-Fetch-Site`/`Origin`.
//!
//! Safe methods (`GET`, `HEAD`, `OPTIONS`, `TRACE`) always pass — CSRF defends writes.
//!
//! Bearer-token APIs do not need this middleware: a cross-site page cannot attach an
//! `Authorization` header it does not possess. Only cookie (or ambient-auth) sessions are
//! forgeable, which is what this layer rejects.
//!
//! ```rust,ignore
//! .layer(Csrf::new(["https://app.example.com"]))
//! ```

use core::future::Future;

use http_kit::{
    header::{HeaderMap, COOKIE, ORIGIN, REFERER},
    http_error, Method, Request, Response, StatusCode, Uri,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error,
};

const SEC_FETCH_SITE: http_kit::header::HeaderName =
    http_kit::header::HeaderName::from_static("sec-fetch-site");

http_error!(
    /// `403 Forbidden` — the request's origin is not trusted for a state-changing method.
    pub CsrfRejected,
    StatusCode::FORBIDDEN,
    "the request did not pass the CSRF origin checks"
);

/// Rejects cross-site state-changing requests.
///
/// `allowed_origins` are compared against `Origin`/`Referer` — scheme, host and port all
/// count, so `https://app.example.com` and `https://other.example.com` stay distinct.
#[derive(Debug, Clone)]
pub struct Csrf {
    allowed_origins: Vec<String>,
    double_submit: Option<(String, String)>,
}

impl Csrf {
    /// An allow-list of origins trusted to send unsafe requests.
    #[must_use]
    pub fn new(origins: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            allowed_origins: origins
                .into_iter()
                .map(|origin| origin.as_ref().trim_end_matches('/').to_owned())
                .collect(),
            double_submit: None,
        }
    }

    /// Admit requests that echo `cookie_name`'s value back in `header_name` — the
    /// double-submit pattern, for legacy clients that never send fetch metadata. A cross-site
    /// attacker can set neither the cookie nor read it, so matching values prove the client
    /// read the cookie the app itself issued.
    #[must_use]
    pub fn allow_double_submit(
        mut self,
        cookie_name: impl Into<String>,
        header_name: impl Into<String>,
    ) -> Self {
        self.double_submit = Some((cookie_name.into(), header_name.into()));
        self
    }

    /// `true` when some configured signal vouches for this request's origin.
    fn passes(&self, request: &Request) -> bool {
        let headers = request.headers();

        // Fetch metadata wins: browsers always send it and it cannot be spoofed by a page.
        // Only `same-origin` and `none` pass on this signal alone — `same-site` still covers
        // sibling origins (potentially hostile tenants on a shared parent domain), so it
        // falls through to the explicit allow-list.
        if let Some(site) = header_str(headers, &SEC_FETCH_SITE) {
            if site.eq_ignore_ascii_case("cross-site") {
                return false;
            }
            if site.eq_ignore_ascii_case("same-origin") || site.eq_ignore_ascii_case("none") {
                return true;
            }
        }

        if let Some(origin) = header_str(headers, &ORIGIN) {
            return self.origin_allowed(origin);
        }

        // Referer carries the full URL; compare its origin part only.
        if let Some(referer) = header_str(headers, &REFERER) {
            return referer
                .parse::<Uri>()
                .ok()
                .and_then(|uri| uri_origin(&uri))
                .is_some_and(|origin| self.origin_allowed(&origin));
        }

        self.double_submit_passes(headers)
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        let origin = origin.trim_end_matches('/');
        self.allowed_origins.iter().any(|allowed| allowed == origin)
    }

    /// Cookie `cookie_name` must exist and equal header `header_name` — byte-constant-time.
    fn double_submit_passes(&self, headers: &HeaderMap) -> bool {
        let Some((cookie_name, header_name)) = &self.double_submit else {
            return false;
        };
        let Some(expected) = headers
            .get(header_name.as_str())
            .and_then(|value| value.to_str().ok())
        else {
            return false;
        };
        header_str(headers, &COOKIE)
            .and_then(|cookies| cookie_value(cookies, cookie_name))
            .is_some_and(|actual| constant_time_eq(actual.as_bytes(), expected.as_bytes()))
    }
}

/// The `scheme://host[:port]` prefix of a URI, the part `Origin` carries.
fn uri_origin(uri: &Uri) -> Option<String> {
    let authority = uri.authority()?;
    let scheme = uri.scheme_str().unwrap_or("http");
    Some(format!("{scheme}://{authority}"))
}

/// `name=value` out of a `Cookie` header's `; `-separated list.
fn cookie_value<'a>(cookies: &'a str, name: &str) -> Option<&'a str> {
    cookies.split(';').map(str::trim).find_map(|pair| {
        pair.split_once('=')
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| value)
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &http_kit::header::HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Length-agnostic constant-time comparison — a mismatch never short-circuits on position.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for index in 0..len {
        diff |= usize::from(a.get(index).unwrap_or(&0) != b.get(index).unwrap_or(&0));
    }
    diff == 0
}

impl Middleware for Csrf {
    fn handle(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> impl Future<Output = Result<Response, Error>> + Send {
        let allowed = is_safe_method(request.method()) || self.passes(request);
        async move {
            if allowed {
                next.run(request).await
            } else {
                Err(Error::from(CsrfRejected::new()))
            }
        }
    }
}

/// `GET`/`HEAD`/`OPTIONS`/`TRACE` — the methods RFC 9110 marks safe.
const fn is_safe_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

#[cfg(test)]
mod tests {
    use super::Csrf;
    use crate::{
        routing::{CreateRouteNode, Route, Router},
        Body, Method, Request, StatusCode,
    };
    use http_kit::header::{HeaderValue, COOKIE, ORIGIN, REFERER};

    const FETCH_SITE: http_kit::header::HeaderName =
        http_kit::header::HeaderName::from_static("sec-fetch-site");

    async fn write() -> &'static str {
        "ok"
    }

    fn post() -> Request {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::POST;
        *request.uri_mut() = "/write".parse().unwrap();
        request
    }

    fn router(layer: Csrf) -> Router {
        Route::new(("/write".post(write),)).layer(layer).build()
    }

    #[tokio::test]
    async fn cross_site_fetch_metadata_rejects() {
        let mut request = post();
        request
            .headers_mut()
            .insert(&FETCH_SITE, HeaderValue::from_static("cross-site"));
        let error = router(Csrf::new(["https://app.example.com"]))
            .go(request)
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn same_site_falls_through_to_the_origin_allow_list() {
        // `same-site` alone admits nothing — a hostile sibling tenant could send it.
        let mut request = post();
        request
            .headers_mut()
            .insert(&FETCH_SITE, HeaderValue::from_static("same-site"));
        let error = router(Csrf::new(Vec::<&str>::new()))
            .go(request)
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);

        // But a same-site request that also proves an allowed Origin passes.
        let mut request = post();
        request
            .headers_mut()
            .insert(&FETCH_SITE, HeaderValue::from_static("same-site"));
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://app.example.com"));
        let response = router(Csrf::new(["https://app.example.com"]))
            .go(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_listed_origin_passes_an_unlisted_one_rejects() {
        let layer = Csrf::new(["https://app.example.com"]);

        let mut request = post();
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://app.example.com"));
        let response = router(layer.clone()).go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut request = post();
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://evil.example.com"));
        let error = router(layer).go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn referer_is_the_fallback_when_origin_is_absent() {
        let mut request = post();
        request.headers_mut().insert(
            REFERER,
            HeaderValue::from_static("https://app.example.com/form?page=1"),
        );
        let response = router(Csrf::new(["https://app.example.com"]))
            .go(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn double_submit_admits_matching_cookie_and_header() {
        let layer = Csrf::new(Vec::<&str>::new()).allow_double_submit("csrf", "x-csrf-token");

        let mut request = post();
        request
            .headers_mut()
            .insert(COOKIE, HeaderValue::from_static("csrf=abc123; other=x"));
        request
            .headers_mut()
            .insert("x-csrf-token", HeaderValue::from_static("abc123"));
        let response = router(layer.clone()).go(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut request = post();
        request
            .headers_mut()
            .insert(COOKIE, HeaderValue::from_static("csrf=abc123"));
        request
            .headers_mut()
            .insert("x-csrf-token", HeaderValue::from_static("different"));
        let error = router(layer).go(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_request_with_no_signals_at_all_rejects() {
        let error = router(Csrf::new(Vec::<&str>::new()))
            .go(post())
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn get_requests_always_pass() {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/write".parse().unwrap();
        // GET to a POST route still reaches the method table — 405, not 403.
        let response = router(Csrf::new(Vec::<&str>::new()))
            .go(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
