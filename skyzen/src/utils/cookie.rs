//! HTTP cookies
pub use cookie::Cookie;

use std::{
    ops::{Deref, DerefMut},
    str::FromStr,
};

use async_trait::async_trait;
use http_kit::{
    header::{self, HeaderValue},
    Request, Response,
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
            jar.add_original(cookie)
        }
        Ok(CookieJar(jar))
    }
}

#[async_trait]
impl Extractor for CookieJar {
    async fn extract(request: &mut Request) -> http_kit::Result<Self> {
        let cookie = request
            .get_header(header::COOKIE)
            .map(|v| v.as_bytes())
            .unwrap_or(&[]);
        let cookies = core::str::from_utf8(cookie)?;
        Ok(cookies.parse()?)
    }
}

impl Responder for CookieJar {
    fn respond_to(self, _request: &Request, response: &mut Response) -> http_kit::Result<()> {
        for cookie in self.0.delta() {
            response.append_header(
                header::SET_COOKIE,
                HeaderValue::try_from(cookie.encoded().to_string())?,
            ); // TODO: reduce unnecessary header value check
        }
        Ok(())
    }
}
