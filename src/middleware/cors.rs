//! Cross-Origin Resource Sharing.

use std::{fmt, time::Duration};

use http_kit::{
    header::{self, HeaderName, HeaderValue},
    Body, Method, Request, Response, StatusCode,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error,
};

/// Which origins a [`Cors`] layer accepts.
#[derive(Debug, Clone)]
pub enum AllowOrigin {
    /// Reflect nothing: every origin is answered with `*`.
    Any,
    /// Accept exactly the listed origins, echoing back the one that matched.
    List(Vec<HeaderValue>),
}

/// Rejected [`Cors`] configurations, reported when the builder is finished.
#[derive(Debug)]
#[non_exhaustive]
pub enum CorsConfigError {
    /// `Access-Control-Allow-Origin: *` cannot be combined with credentials, per the Fetch
    /// standard: a browser rejects the response instead of sending the cookie.
    CredentialsWithAnyOrigin,
    /// An origin, method or header value was not valid for an HTTP header.
    InvalidValue {
        /// Which builder input was rejected.
        field: &'static str,
        /// The offending value.
        value: String,
    },
    /// No origin was listed, so the layer could never accept a cross-origin request.
    NoOrigin,
}

impl fmt::Display for CorsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CredentialsWithAnyOrigin => f.write_str(
                "`allow_credentials(true)` cannot be combined with `allow_origin_any()`; \
                 list the permitted origins explicitly",
            ),
            Self::InvalidValue { field, value } => {
                write!(f, "`{field}` value `{value}` is not a valid header value")
            }
            Self::NoOrigin => f.write_str(
                "no allowed origin configured; call `allow_origin`, `allow_origins` or \
                 `allow_origin_any`",
            ),
        }
    }
}

impl std::error::Error for CorsConfigError {}

/// Answers CORS preflight requests and decorates cross-origin responses.
///
/// Attach it with [`Route::layer`](crate::routing::Route::layer) rather than
/// [`Route::with`](crate::routing::Route::with): a preflight `OPTIONS` arrives at a path whose
/// registered methods are `GET`/`POST`, so it must be seen *before* the router synthesizes its
/// `405`, and only a router layer runs that early.
///
/// ```rust
/// use skyzen::{middleware::Cors, routing::{CreateRouteNode, Route}, Result};
///
/// let router = Route::new((
///     "/items".at(|| async { Result::Ok("[]") }),
/// ))
/// .layer(
///     Cors::builder()
///         .allow_origin("https://app.lexo.cool")
///         .allow_methods([skyzen::Method::GET, skyzen::Method::POST])
///         .allow_credentials(true)
///         .build()
///         .expect("valid CORS configuration"),
/// )
/// .build();
/// ```
#[derive(Debug, Clone)]
pub struct Cors {
    origins: AllowOrigin,
    methods: HeaderValue,
    allowed_headers: Option<HeaderValue>,
    exposed_headers: Option<HeaderValue>,
    credentials: bool,
    max_age: Option<HeaderValue>,
}

impl Cors {
    /// Start configuring a CORS layer.
    #[must_use]
    pub fn builder() -> CorsBuilder {
        CorsBuilder::default()
    }

    /// A permissive layer: any origin, the common methods, and any requested headers.
    ///
    /// Credentials are off, which is the only combination a browser accepts alongside `*`.
    ///
    /// # Panics
    ///
    /// Never: the configuration is valid by construction.
    #[must_use]
    pub fn permissive() -> Self {
        Self::builder()
            .allow_origin_any()
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::HEAD,
                Method::OPTIONS,
            ])
            .allow_headers_any()
            .build()
            .expect("the permissive configuration is valid")
    }

    /// The `Access-Control-Allow-Origin` value to answer `origin` with, if it is accepted.
    fn allow_origin_for(&self, origin: Option<&HeaderValue>) -> Option<HeaderValue> {
        match &self.origins {
            AllowOrigin::Any => Some(HeaderValue::from_static("*")),
            AllowOrigin::List(allowed) => {
                let origin = origin?;
                allowed
                    .iter()
                    .find(|candidate| *candidate == origin)
                    .cloned()
            }
        }
    }

    /// Whether the response varies by `Origin`, and so must not be cached across origins.
    const fn varies_by_origin(&self) -> bool {
        matches!(self.origins, AllowOrigin::List(_))
    }

    fn decorate(&self, headers: &mut header::HeaderMap, allow_origin: HeaderValue) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow_origin);
        if self.credentials {
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
        if let Some(exposed) = &self.exposed_headers {
            headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, exposed.clone());
        }
        if self.varies_by_origin() {
            headers.append(header::VARY, HeaderValue::from_static("origin"));
        }
    }

    /// Build the `204` answer to a preflight request.
    fn preflight(&self, request: &Request, allow_origin: HeaderValue) -> Response {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        let requested_headers = request
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .cloned();
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, self.methods.clone());
        // `allow_headers_any` echoes what the browser asked for, which is the only form that
        // works alongside credentials.
        if let Some(allowed) = self.allowed_headers.clone().or(requested_headers) {
            headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, allowed);
        }
        if let Some(max_age) = &self.max_age {
            headers.insert(header::ACCESS_CONTROL_MAX_AGE, max_age.clone());
        }
        self.decorate(headers, allow_origin);
        response
    }
}

/// Whether a request is a CORS preflight: `OPTIONS` carrying `Access-Control-Request-Method`.
fn is_preflight(request: &Request) -> bool {
    request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

impl Middleware for Cors {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let origin = request.headers().get(header::ORIGIN).cloned();
        let Some(allow_origin) = self.allow_origin_for(origin.as_ref()) else {
            // Not an accepted origin (or not a cross-origin request at all): pass through
            // untouched so a same-origin caller sees the normal response.
            return next.run(request).await;
        };

        if is_preflight(request) {
            return Ok(self.preflight(request, allow_origin));
        }

        let mut response = next.run(request).await?;
        self.decorate(response.headers_mut(), allow_origin);
        Ok(response)
    }
}

/// Builder for [`Cors`].
#[derive(Debug, Default)]
pub struct CorsBuilder {
    origins: Option<AllowOrigin>,
    methods: Vec<Method>,
    allowed_headers: Option<Vec<HeaderName>>,
    allow_headers_any: bool,
    exposed_headers: Vec<HeaderName>,
    credentials: bool,
    max_age: Option<Duration>,
    invalid: Option<CorsConfigError>,
}

impl CorsBuilder {
    /// Accept one origin, such as `https://app.lexo.cool`.
    #[must_use]
    pub fn allow_origin(self, origin: &str) -> Self {
        self.allow_origins([origin])
    }

    /// Accept each of the listed origins.
    #[must_use]
    pub fn allow_origins<'a>(mut self, origins: impl IntoIterator<Item = &'a str>) -> Self {
        let mut values = match self.origins.take() {
            Some(AllowOrigin::List(values)) => values,
            _ => Vec::new(),
        };
        for origin in origins {
            match HeaderValue::from_str(origin) {
                Ok(value) => values.push(value),
                Err(_) => {
                    self.invalid.get_or_insert(CorsConfigError::InvalidValue {
                        field: "allow_origin",
                        value: origin.to_owned(),
                    });
                }
            }
        }
        self.origins = Some(AllowOrigin::List(values));
        self
    }

    /// Accept every origin, answering with `*`.
    ///
    /// Incompatible with [`allow_credentials(true)`](Self::allow_credentials); [`build`](Self::build)
    /// rejects that combination.
    #[must_use]
    pub fn allow_origin_any(mut self) -> Self {
        self.origins = Some(AllowOrigin::Any);
        self
    }

    /// The methods a cross-origin caller may use.
    #[must_use]
    pub fn allow_methods(mut self, methods: impl IntoIterator<Item = Method>) -> Self {
        self.methods.extend(methods);
        self
    }

    /// The request headers a cross-origin caller may send.
    #[must_use]
    pub fn allow_headers(mut self, headers: impl IntoIterator<Item = HeaderName>) -> Self {
        self.allowed_headers
            .get_or_insert_with(Vec::new)
            .extend(headers);
        self
    }

    /// Allow whatever headers the preflight asks for, by echoing the request back.
    #[must_use]
    pub const fn allow_headers_any(mut self) -> Self {
        self.allow_headers_any = true;
        self
    }

    /// Response headers the caller's script is allowed to read.
    #[must_use]
    pub fn expose_headers(mut self, headers: impl IntoIterator<Item = HeaderName>) -> Self {
        self.exposed_headers.extend(headers);
        self
    }

    /// Whether cookies and `Authorization` headers may accompany the request.
    #[must_use]
    pub const fn allow_credentials(mut self, allow: bool) -> Self {
        self.credentials = allow;
        self
    }

    /// How long a browser may cache the preflight result.
    #[must_use]
    pub const fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = Some(max_age);
        self
    }

    /// Finish the configuration.
    ///
    /// # Errors
    ///
    /// Returns [`CorsConfigError`] when no origin was configured, when a value could not be
    /// rendered as a header, or when credentials were combined with a wildcard origin — a
    /// combination browsers reject at runtime, so it is rejected here instead.
    pub fn build(self) -> Result<Cors, CorsConfigError> {
        if let Some(error) = self.invalid {
            return Err(error);
        }

        let origins = self.origins.ok_or(CorsConfigError::NoOrigin)?;
        match &origins {
            AllowOrigin::Any if self.credentials => {
                return Err(CorsConfigError::CredentialsWithAnyOrigin);
            }
            AllowOrigin::List(values) if values.is_empty() => {
                return Err(CorsConfigError::NoOrigin);
            }
            _ => {}
        }

        let methods = join_header(self.methods.iter().map(Method::as_str), "allow_methods")?;
        let allowed_headers = if self.allow_headers_any {
            None
        } else {
            self.allowed_headers
                .as_ref()
                .map(|headers| join_header(headers.iter().map(HeaderName::as_str), "allow_headers"))
                .transpose()?
        };
        let exposed_headers = if self.exposed_headers.is_empty() {
            None
        } else {
            Some(join_header(
                self.exposed_headers.iter().map(HeaderName::as_str),
                "expose_headers",
            )?)
        };
        let max_age = self
            .max_age
            .map(|max_age| {
                HeaderValue::from_str(&max_age.as_secs().to_string()).map_err(|_| {
                    CorsConfigError::InvalidValue {
                        field: "max_age",
                        value: max_age.as_secs().to_string(),
                    }
                })
            })
            .transpose()?;

        Ok(Cors {
            origins,
            methods,
            allowed_headers,
            exposed_headers,
            credentials: self.credentials,
            max_age,
        })
    }
}

/// Render a comma-separated header list, reporting `field` if the result is not a valid value.
fn join_header<'a>(
    values: impl Iterator<Item = &'a str>,
    field: &'static str,
) -> Result<HeaderValue, CorsConfigError> {
    let joined = values.collect::<Vec<_>>().join(", ");
    HeaderValue::from_str(&joined).map_err(|_| CorsConfigError::InvalidValue {
        field,
        value: joined,
    })
}

#[cfg(test)]
mod tests {
    use super::{Cors, CorsConfigError};
    use crate::{
        header,
        header::HeaderValue,
        routing::{CreateRouteNode, Route},
        Body, Method, Request, Result, StatusCode,
    };

    const APP: &str = "https://app.lexo.cool";

    fn preflight(path: &str, requested: Method) -> Request {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::OPTIONS;
        *request.uri_mut() = path.parse().expect("valid path");
        let headers = request.headers_mut();
        headers.insert(header::ORIGIN, HeaderValue::from_static(APP));
        headers.insert(
            header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_str(requested.as_str()).expect("valid method"),
        );
        request
    }

    fn cors() -> Cors {
        Cors::builder()
            .allow_origin(APP)
            .allow_methods([Method::GET, Method::DELETE])
            .allow_credentials(true)
            .build()
            .expect("valid configuration")
    }

    #[tokio::test]
    async fn preflight_on_an_unregistered_method_still_gets_cors_headers() {
        // `/items` registers GET only, so without a router layer this preflight would be
        // answered by the router's own 405 and carry no `Access-Control-*` headers at all.
        let router = Route::new(("/items".at(|| async { Result::Ok("[]") }),))
            .layer(cors())
            .build();

        let response = router
            .go(preflight("/items", Method::DELETE))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            APP
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .unwrap(),
            "GET, DELETE"
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
    }

    #[tokio::test]
    async fn preflight_on_an_unmatched_path_is_answered_instead_of_404() {
        let router = Route::new(("/items".at(|| async { Result::Ok("[]") }),))
            .layer(cors())
            .build();

        let response = router.go(preflight("/missing", Method::GET)).await.unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    }

    #[tokio::test]
    async fn normal_responses_are_decorated_and_vary_by_origin() {
        let router = Route::new(("/items".at(|| async { Result::Ok("[]") }),))
            .layer(cors())
            .build();

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/items".parse().unwrap();
        request
            .headers_mut()
            .insert(header::ORIGIN, HeaderValue::from_static(APP));

        let response = router.go(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            APP
        );
        assert_eq!(response.headers().get(header::VARY).unwrap(), "origin");
    }

    #[tokio::test]
    async fn an_unlisted_origin_is_left_undecorated() {
        let router = Route::new(("/items".at(|| async { Result::Ok("[]") }),))
            .layer(cors())
            .build();

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/items".parse().unwrap();
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.lexo.cool"),
        );

        let response = router.go(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    }

    #[test]
    fn credentials_with_a_wildcard_origin_is_rejected_at_construction() {
        let error = Cors::builder()
            .allow_origin_any()
            .allow_credentials(true)
            .build()
            .unwrap_err();
        assert!(matches!(error, CorsConfigError::CredentialsWithAnyOrigin));
    }

    #[test]
    fn a_layer_without_an_origin_is_rejected() {
        let error = Cors::builder().build().unwrap_err();
        assert!(matches!(error, CorsConfigError::NoOrigin));
    }
}
