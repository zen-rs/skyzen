//! HTTP cookies
pub use cookie::Cookie;
use http::StatusCode;

use std::{
    ops::{Deref, DerefMut},
    str::FromStr,
};

use http_kit::{
    header::{self, HeaderValue},
    http_error, Request, Response,
};
use skyzen_core::{Extractor, Responder};

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
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        // HTTP/2 allows the Cookie header to be split into several field lines,
        // so join every occurrence with "; " before parsing.
        let mut combined = String::new();
        for value in request.headers().get_all(header::COOKIE) {
            let value =
                core::str::from_utf8(value.as_bytes()).map_err(|_| CookieParseError::new())?;
            if !combined.is_empty() {
                combined.push_str("; ");
            }
            combined.push_str(value);
        }
        combined
            .parse::<Self>()
            .map_err(|_| CookieParseError::new())
    }
}

http_error!(
    /// Error occurs when setting cookies to response headers.
    pub CookieSetError, StatusCode::INTERNAL_SERVER_ERROR, "Failed to set cookies"
);

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
    use skyzen_core::{Extractor, Responder};

    use super::{Cookie, CookieJar};
    use crate::{Body, Request, Response, StatusCode};

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
}
