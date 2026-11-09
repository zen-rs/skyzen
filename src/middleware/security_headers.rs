//! `SecurityHeaders` — the standard hardening response headers, in one layer.

use core::future::Future;
use std::time::Duration;

use http_kit::{
    header::{HeaderName, HeaderValue},
    Request, Response,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error,
};

const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");
const X_FRAME_OPTIONS: HeaderName = HeaderName::from_static("x-frame-options");
const REFERRER_POLICY: HeaderName = HeaderName::from_static("referrer-policy");
const STRICT_TRANSPORT_SECURITY: HeaderName = HeaderName::from_static("strict-transport-security");
const CROSS_ORIGIN_OPENER_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-opener-policy");
const CROSS_ORIGIN_RESOURCE_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-resource-policy");
const PERMISSIONS_POLICY: HeaderName = HeaderName::from_static("permissions-policy");
const CONTENT_SECURITY_POLICY: HeaderName = HeaderName::from_static("content-security-policy");

/// Emits the standard security response headers on every response — including 4xx/5xx and
/// 404/405 paths when applied with `.layer`.
///
/// The preset covers what a JSON API needs and nothing it doesn't: `nosniff`, no framing, a
/// conservative referrer policy, and an isolated browsing-context group. Headers the handler
/// already set are **never overwritten**, so a route can always relax the policy — an HTML
/// docs page sets its own CSP, a download route its own `Cross-Origin-Resource-Policy`.
///
/// ```rust,ignore
/// .layer(SecurityHeaders::new()
///     .hsts()
///     .csp_api()
///     .permissions_policy("camera=(), microphone=(), geolocation=()"))
/// ```
#[derive(Debug, Clone)]
pub struct SecurityHeaders {
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        Self::new()
    }
}

impl SecurityHeaders {
    /// Secure defaults: `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`,
    /// `Referrer-Policy: strict-origin-when-cross-origin`,
    /// `Cross-Origin-Opener-Policy: same-origin`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            headers: vec![
                (X_CONTENT_TYPE_OPTIONS, value("nosniff")),
                (X_FRAME_OPTIONS, value("DENY")),
                (REFERRER_POLICY, value("strict-origin-when-cross-origin")),
                (CROSS_ORIGIN_OPENER_POLICY, value("same-origin")),
            ],
        }
    }

    /// `Strict-Transport-Security` for `max_age`, `includeSubDomains` and `preload` ready.
    ///
    /// Opt-in because it is sticky: a client that sees it refuses plain HTTP until the
    /// max-age lapses. Only enable on deployments that are HTTPS-only for good.
    #[must_use]
    pub fn hsts_with_max_age(self, max_age: Duration) -> Self {
        self.set(
            STRICT_TRANSPORT_SECURITY,
            value(&format!(
                "max-age={}; includeSubDomains; preload",
                max_age.as_secs()
            )),
        )
    }

    /// `Strict-Transport-Security` for one year — see [`hsts_with_max_age`][Self::hsts_with_max_age].
    #[must_use]
    pub fn hsts(self) -> Self {
        self.hsts_with_max_age(Duration::from_hours(8760))
    }

    /// `Content-Security-Policy` — `default-src 'none'; frame-ancestors 'none'`, the preset
    /// for a JSON API that serves no browser content.
    #[must_use]
    pub fn csp_api(self) -> Self {
        self.set(
            CONTENT_SECURITY_POLICY,
            value("default-src 'none'; frame-ancestors 'none'"),
        )
    }

    /// `Content-Security-Policy` with a policy of your own — the docs page wants
    /// `default-src 'self'` plus whatever its CDN bundle needs.
    #[must_use]
    pub fn csp(self, policy: impl AsRef<str>) -> Self {
        self.set(CONTENT_SECURITY_POLICY, value(policy.as_ref()))
    }

    /// Replace `X-Frame-Options` — `SAMEORIGIN` for pages that frame themselves.
    #[must_use]
    pub fn frame_options(self, policy: impl AsRef<str>) -> Self {
        self.set(X_FRAME_OPTIONS, value(policy.as_ref()))
    }

    /// Replace the `Referrer-Policy`.
    #[must_use]
    pub fn referrer_policy(self, policy: impl AsRef<str>) -> Self {
        self.set(REFERRER_POLICY, value(policy.as_ref()))
    }

    /// `Cross-Origin-Opener-Policy` — replaces the `same-origin` default.
    #[must_use]
    pub fn cross_origin_opener_policy(self, policy: impl AsRef<str>) -> Self {
        self.set(CROSS_ORIGIN_OPENER_POLICY, value(policy.as_ref()))
    }

    /// `Cross-Origin-Resource-Policy` — `cross-origin` for assets other sites legitimately
    /// embed, `same-site` or the `same-origin` absent from the defaults.
    #[must_use]
    pub fn cross_origin_resource_policy(self, policy: impl AsRef<str>) -> Self {
        self.set(CROSS_ORIGIN_RESOURCE_POLICY, value(policy.as_ref()))
    }

    /// `Permissions-Policy` — e.g. `camera=(), microphone=(), geolocation=()`.
    #[must_use]
    pub fn permissions_policy(self, policy: impl AsRef<str>) -> Self {
        self.set(PERMISSIONS_POLICY, value(policy.as_ref()))
    }

    /// Any other hardening header, set the same never-overwrite way.
    ///
    /// # Panics
    /// `value` must be a legal header value — a `&'static str` literal always is.
    #[must_use]
    pub fn header(self, name: &'static str, value: &'static str) -> Self {
        self.set(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        )
    }

    /// Set-or-replace in the policy list, so the last builder call wins at emit time.
    fn set(mut self, name: HeaderName, value: HeaderValue) -> Self {
        match self.headers.iter_mut().find(|(key, _)| *key == name) {
            Some((_, existing)) => *existing = value,
            None => self.headers.push((name, value)),
        }
        self
    }
}

fn value(text: &str) -> HeaderValue {
    HeaderValue::try_from(text).expect("a hardcoded policy is a valid header value")
}

impl Middleware for SecurityHeaders {
    fn handle(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> impl Future<Output = Result<Response, Error>> + Send {
        let headers = self.headers.clone();
        async move {
            let mut response = next.run(request).await?;
            for (name, value) in headers {
                response
                    .headers_mut()
                    .entry(name)
                    .or_insert_with(|| value.clone());
            }
            Ok(response)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SecurityHeaders;
    use crate::{
        routing::{CreateRouteNode, Route},
        Body, Request, StatusCode,
    };
    use http_kit::header::{HeaderName, HeaderValue};
    use std::time::Duration;

    async fn hello() -> &'static str {
        "hello"
    }

    async fn framed() -> (HeaderName, HeaderValue) {
        (
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("SAMEORIGIN"),
        )
    }

    #[tokio::test]
    async fn defaults_land_on_every_response() {
        let router = Route::new(("/hello".at(hello),))
            .layer(SecurityHeaders::new())
            .build();
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/hello".parse().unwrap();
        let response = router.go(request).await.unwrap();
        let headers = response.headers();
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(
            headers["referrer-policy"],
            "strict-origin-when-cross-origin"
        );
        assert_eq!(headers["cross-origin-opener-policy"], "same-origin");
        // Opt-ins stay off until asked for.
        assert!(headers.get("strict-transport-security").is_none());
        assert!(headers.get("content-security-policy").is_none());
    }

    #[tokio::test]
    async fn a_handler_set_header_is_never_overwritten() {
        let router = Route::new(("/framed".at(framed),))
            .layer(SecurityHeaders::new())
            .build();
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/framed".parse().unwrap();
        let response = router.go(request).await.unwrap();
        assert_eq!(response.headers()["x-frame-options"], "SAMEORIGIN");
    }

    #[tokio::test]
    async fn opt_ins_land_when_built() {
        let router = Route::new(("/hello".at(hello),))
            .layer(
                SecurityHeaders::new()
                    .hsts_with_max_age(Duration::from_secs(300))
                    .csp_api()
                    .permissions_policy("camera=()"),
            )
            .build();
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/hello".parse().unwrap();
        let response = router.go(request).await.unwrap();
        let headers = response.headers();
        assert_eq!(
            headers["strict-transport-security"],
            "max-age=300; includeSubDomains; preload"
        );
        assert_eq!(
            headers["content-security-policy"],
            "default-src 'none'; frame-ancestors 'none'"
        );
        assert_eq!(headers["permissions-policy"], "camera=()");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn builder_last_call_wins() {
        let layer = SecurityHeaders::new().frame_options("SAMEORIGIN");
        let router = Route::new(("/hello".at(hello),)).layer(layer).build();
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/hello".parse().unwrap();
        let response = router.go(request).await.unwrap();
        assert_eq!(response.headers()["x-frame-options"], "SAMEORIGIN");
    }
}
