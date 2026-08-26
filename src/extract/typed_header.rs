//! RFC-typed request and response headers, backed by the [`headers`] crate.

use core::convert::Infallible;

use headers::{Header, HeaderMapExt};

use crate::{extract::Extractor, responder::Responder, Request, Response, StatusCode};

/// Reads or writes one header through its typed representation.
///
/// The [`headers`] crate owns the parsing and rendering, so `Accept`, `Range`, `If-None-Match`,
/// `Authorization<Basic>`, `Cache-Control` and the rest arrive already validated instead of as raw
/// bytes.
///
/// ```rust
/// use skyzen::{extract::TypedHeader, headers::UserAgent, Result};
///
/// async fn whoami(TypedHeader(agent): TypedHeader<UserAgent>) -> Result<String> {
///     Ok(agent.as_str().to_owned())
/// }
/// ```
///
/// As a responder it appends the header to the response, so it composes in a tuple with a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypedHeader<H>(pub H);

impl_deref!(TypedHeader);

/// Raised when a typed header is absent or unparseable.
#[skyzen::error]
pub enum TypedHeaderError {
    /// The request carries no such header.
    #[error("Missing required header `{0}`", status = StatusCode::BAD_REQUEST)]
    Missing(String),
    /// The header is present but does not parse as its declared type.
    #[error("Header `{0}` is malformed", status = StatusCode::BAD_REQUEST)]
    Invalid(String),
}

impl<H: Header + Send + Sync + 'static> Extractor for TypedHeader<H> {
    type Error = TypedHeaderError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let name = H::name();
        let all = request.headers().get_all(name);
        // `Header::decode` cannot tell "absent" from "unparseable" — it reports the same error for
        // both — so absence is checked first, and the two get distinct messages.
        if all.iter().next().is_none() {
            return Err(TypedHeaderError::Missing(name.to_string()));
        }
        H::decode(&mut all.iter())
            .map(Self)
            .map_err(|_| TypedHeaderError::Invalid(name.to_string()))
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            location: crate::openapi::ParameterLocation::Header,
            content_type: None,
            schema: Some(skyzen_core::openapi::plain_string_schema()),
        })
    }
}

impl<H: Header + Send + Sync + 'static> Responder for TypedHeader<H> {
    type Error = Infallible;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        response.headers_mut().typed_insert(self.0);
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<crate::openapi::ResponseSchema>> {
        Some(vec![crate::openapi::ResponseSchema {
            status: None,
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::{TypedHeader, TypedHeaderError};
    use crate::{header::HeaderValue, Body, Request, Response, StatusCode};
    use headers::{ContentType, UserAgent};
    use http_kit::HttpError;
    use skyzen_core::{Extractor, Responder};

    #[tokio::test]
    async fn reads_a_typed_request_header() {
        let mut request = Request::new(Body::empty());
        request.headers_mut().insert(
            crate::header::USER_AGENT,
            HeaderValue::from_static("skyzen/1"),
        );

        let TypedHeader(agent) = TypedHeader::<UserAgent>::extract(&mut request)
            .await
            .unwrap();
        assert_eq!(agent.as_str(), "skyzen/1");
    }

    #[tokio::test]
    async fn an_absent_header_names_itself() {
        let mut request = Request::new(Body::empty());
        let error = TypedHeader::<UserAgent>::extract(&mut request)
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(matches!(error, TypedHeaderError::Missing(ref name) if name == "user-agent"));
    }

    #[tokio::test]
    async fn a_malformed_header_is_distinguished_from_an_absent_one() {
        let mut request = Request::new(Body::empty());
        request.headers_mut().insert(
            crate::header::CONTENT_TYPE,
            HeaderValue::from_static("not a media type"),
        );

        let error = TypedHeader::<ContentType>::extract(&mut request)
            .await
            .unwrap_err();
        assert!(matches!(error, TypedHeaderError::Invalid(_)), "{error}");
    }

    #[test]
    fn writes_a_typed_response_header() {
        let mut response = Response::new(Body::empty());
        TypedHeader(ContentType::json())
            .respond_to(&Request::new(Body::empty()), &mut response)
            .expect("typed headers always render");
        assert_eq!(
            response.headers().get(crate::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }
}
