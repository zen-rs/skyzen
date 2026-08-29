//! Form utilities module.

use crate::{
    extract::Extractor,
    header::{HeaderValue, CONTENT_TYPE},
    responder::Responder,
    Method, Request, Response, StatusCode,
};

use http_kit::http_error;
use serde::{de::DeserializeOwned, Serialize};
use serde_html_form::{from_str, to_string};
use skyzen_core::{take_body_bytes, BodyExtractorError};

/// Extract form from request body.
///
/// Repeated keys collect into a sequence in both directions, so a `tags: Vec<String>` field
/// round-trips as `tags=a&tags=b`.
#[derive(Debug)]
pub struct Form<T: Send + Sync>(pub T);

/// What a [`Form`] extraction can fail with: the body was unavailable, or it was not form data.
pub type FormRejection = BodyExtractorError<FormContentTypeError>;

#[allow(clippy::declare_interior_mutable_const)]
const APPLICATION_WWW_FORM_URLENCODED: HeaderValue =
    HeaderValue::from_static("application/x-www-form-urlencoded");

http_error!(
    /// Raised when the response body could not be encoded as form data.
    pub FormEncodeError, StatusCode::INTERNAL_SERVER_ERROR, "Failed to encode form data"
);

impl<T: Send + Sync + Serialize + crate::ToSchema + 'static> Responder for Form<T> {
    type Error = FormEncodeError;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        let encoded = to_string(&self.0).map_err(|_| FormEncodeError::new())?;
        *response.body_mut() = http_kit::Body::from_bytes(encoded);
        response
            .headers_mut()
            .insert(CONTENT_TYPE, APPLICATION_WWW_FORM_URLENCODED);
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<crate::openapi::ResponseSchema>> {
        Some(vec![crate::openapi::ResponseSchema {
            status: None,
            description: None,
            schema: crate::openapi::schema_of::<T>(),
            content_type: Some("application/x-www-form-urlencoded"),
        }])
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
        crate::openapi::register_schema_for::<T>(defs);
    }
}

/// Errors raised when parsing `application/x-www-form-urlencoded` data.
#[skyzen::error]
pub enum FormContentTypeError {
    /// The content type header is missing.
    #[error(
        "Expected content type `application/x-www-form-urlencoded`",
        status = StatusCode::BAD_REQUEST
    )]
    Missing,
    /// The content type does not match `application/x-www-form-urlencoded`.
    #[error(
        "Expected content type `application/x-www-form-urlencoded`",
        status = StatusCode::UNSUPPORTED_MEDIA_TYPE
    )]
    Unsupported,
    /// The payload could not be parsed as form data.
    ///
    /// Carries the deserializer's own message so the 4xx response tells the caller which field
    /// was wrong, rather than only saying that something was.
    #[error("Failed to parse form data: {0}", status = StatusCode::BAD_REQUEST)]
    InvalidPayload(String),
}

impl<T: Send + Sync + DeserializeOwned + crate::ToSchema + 'static> Extractor for Form<T> {
    type Error = FormRejection;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        // A GET request carries the form in the query string, so it needs no content type — and
        // reads no body, which is why it neither consumes the body nor consults the size limit.
        if request.method() == Method::GET {
            let data = request.uri().query().unwrap_or_default();
            return extract(data).map_err(FormRejection::Parse);
        }

        if let Some(content_type) = request.headers().get(CONTENT_TYPE) {
            if !is_form_content_type(content_type) {
                return Err(FormRejection::Parse(FormContentTypeError::Unsupported));
            }
        } else {
            return Err(FormRejection::Parse(FormContentTypeError::Missing));
        }

        let bytes = take_body_bytes::<Self>(request).await?;
        let data = core::str::from_utf8(&bytes).map_err(|error| {
            tracing::debug!(%error, "form request body is not valid UTF-8");
            FormRejection::Parse(FormContentTypeError::InvalidPayload(error.to_string()))
        })?;
        extract(data).map_err(FormRejection::Parse)
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            location: crate::openapi::ParameterLocation::Body,
            content_type: Some("application/x-www-form-urlencoded"),
            schema: crate::openapi::schema_of::<T>(),
        })
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
        crate::openapi::register_schema_for::<T>(defs);
    }
}

fn extract<T: Send + Sync + DeserializeOwned>(data: &str) -> Result<Form<T>, FormContentTypeError> {
    from_str(data).map(Form).map_err(|error| {
        tracing::debug!(%error, "failed to deserialize form data");
        FormContentTypeError::InvalidPayload(error.to_string())
    })
}

impl_deref!(Form);

fn is_form_content_type(value: &HeaderValue) -> bool {
    value
        .to_str()
        .ok()
        .and_then(|raw| raw.split(';').next())
        .is_some_and(|mime| {
            mime.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
}

#[cfg(test)]
mod tests {
    use super::{Form, FormContentTypeError, FormRejection};
    use crate::{Body, Method, Responder, Response, StatusCode};
    use http_kit::{header::CONTENT_TYPE, HttpError, Request};
    use serde::{Deserialize, Serialize};
    use skyzen_core::{Extractor, RequestBodyLimit};

    #[derive(Debug, Deserialize, PartialEq, crate::ToSchema)]
    struct Payload {
        name: String,
        age: u8,
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq, crate::ToSchema)]
    struct Tagged {
        tags: Vec<String>,
    }

    /// The parse half of a rejection, so a test can name the variant it expects.
    fn parse_error(error: FormRejection) -> FormContentTypeError {
        match error {
            FormRejection::Parse(error) => error,
            FormRejection::Body(error) => panic!("expected a parse rejection, got {error}"),
        }
    }

    fn request_with_body(body: &'static [u8]) -> Request {
        let mut request = Request::new(Body::from_bytes(body.to_vec()));
        *request.method_mut() = Method::POST;
        *request.uri_mut() = "http://localhost/".parse().expect("invalid uri");
        request
    }

    #[tokio::test]
    async fn accepts_charset_param() {
        let mut request = request_with_body(b"name=Lexo&age=17");
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static(
                "application/x-www-form-urlencoded; charset=utf-8",
            ),
        );

        let Form(payload) = Form::<Payload>::extract(&mut request)
            .await
            .expect("form should parse");
        assert_eq!(
            payload,
            Payload {
                name: "Lexo".to_string(),
                age: 17
            }
        );
    }

    #[tokio::test]
    async fn rejects_missing_content_type_on_body() {
        let mut request = request_with_body(b"name=Lexo&age=17");
        let error = Form::<Payload>::extract(&mut request).await.unwrap_err();
        assert!(matches!(parse_error(error), FormContentTypeError::Missing));
    }

    #[tokio::test]
    async fn rejects_invalid_payload() {
        let mut request = request_with_body(b"name=Lexo&age=oops");
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let error = parse_error(Form::<Payload>::extract(&mut request).await.unwrap_err());
        assert!(matches!(error, FormContentTypeError::InvalidPayload(_)));
        let message = error.to_string();
        assert!(
            message.starts_with("Failed to parse form data: "),
            "rejection should keep its prefix, got {message}"
        );
        assert!(
            message.contains("invalid digit"),
            "rejection should carry the deserializer detail, got {message}"
        );
    }

    #[tokio::test]
    async fn rejects_wrong_content_type() {
        let mut request = request_with_body(b"name=Lexo&age=17");
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("text/plain"),
        );
        let error = Form::<Payload>::extract(&mut request).await.unwrap_err();
        assert!(matches!(
            parse_error(error),
            FormContentTypeError::Unsupported
        ));
    }

    #[tokio::test]
    async fn parses_get_query_without_content_type() {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::GET;
        *request.uri_mut() = "http://localhost/?name=Lexo&age=17"
            .parse()
            .expect("invalid uri");

        let Form(payload) = Form::<Payload>::extract(&mut request)
            .await
            .expect("query form should parse");
        assert_eq!(
            payload,
            Payload {
                name: "Lexo".to_string(),
                age: 17
            }
        );
    }

    #[tokio::test]
    async fn repeated_keys_round_trip_through_a_sequence() {
        let mut request = request_with_body(b"tags=a&tags=b");
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );

        let Form(payload) = Form::<Tagged>::extract(&mut request)
            .await
            .expect("repeated keys should collect");
        assert_eq!(payload.tags, ["a".to_owned(), "b".to_owned()]);

        let mut response = Response::new(Body::empty());
        Form(payload)
            .respond_to(&Request::new(Body::empty()), &mut response)
            .expect("sequence should encode");
        let encoded = response.into_body().into_string().await.unwrap();
        assert_eq!(encoded, "tags=a&tags=b");
    }

    #[tokio::test]
    async fn an_oversized_form_body_is_rejected_with_413() {
        let mut request = request_with_body(b"name=Lexo&age=17");
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        request.extensions_mut().insert(RequestBodyLimit::new(4));

        let error = Form::<Payload>::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
