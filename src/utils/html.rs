//! HTML responses.

use core::convert::Infallible;

use crate::{
    header::{HeaderValue, CONTENT_TYPE},
    Body, Request, Response,
};
use skyzen_core::Responder;

#[allow(clippy::declare_interior_mutable_const)]
const TEXT_HTML: HeaderValue = HeaderValue::from_static("text/html; charset=utf-8");

/// Sends its payload as `text/html; charset=utf-8`.
///
/// Without it a `String` returned from a handler is served as plain text, so the browser shows the
/// markup instead of rendering it.
///
/// ```rust
/// use skyzen::{utils::Html, Result};
///
/// async fn page() -> Result<Html<String>> {
///     Ok(Html("<h1>Hello</h1>".to_owned()))
/// }
/// ```
///
/// The payload is inserted verbatim: escape anything that came from a caller before it reaches
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Html<T>(pub T);

impl_deref!(Html);

impl<T: Into<Body> + Send + Sync + 'static> Responder for Html<T> {
    type Error = Infallible;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        *response.body_mut() = self.0.into();
        response.headers_mut().insert(CONTENT_TYPE, TEXT_HTML);
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<crate::openapi::ResponseSchema>> {
        Some(vec![crate::openapi::ResponseSchema {
            status: None,
            description: None,
            schema: Some(skyzen_core::openapi::plain_string_schema()),
            content_type: Some("text/html; charset=utf-8"),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::Html;
    use crate::{header::CONTENT_TYPE, Body, Request, Response};
    use skyzen_core::Responder;

    #[tokio::test]
    async fn sets_the_html_content_type() {
        let mut response = Response::new(Body::empty());
        Html("<h1>Hi</h1>")
            .respond_to(&Request::new(Body::empty()), &mut response)
            .expect("html always responds");

        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "<h1>Hi</h1>");
    }
}
