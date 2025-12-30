//! Form utilities module.

use crate::{
    extract::Extractor,
    header::{HeaderValue, CONTENT_TYPE},
    responder::Responder,
    Method, Request, Response, StatusCode,
};

use http_kit::http_error;
use serde::{de::DeserializeOwned, Serialize};
use serde_urlencoded::from_str;

/// Extract form from request body.
#[derive(Debug)]
pub struct Form<T: Send + Sync>(pub T);

#[allow(clippy::declare_interior_mutable_const)]
const APPLICATION_WWW_FORM_URLENCODED: HeaderValue =
    HeaderValue::from_static("application/x-www-form-urlencoded");

http_error!(
    /// Raised when the request content-type is not `application/x-www-form-urlencoded`.
    pub FormEncodeError, StatusCode::SERVICE_UNAVAILABLE, "Failed to parse form data"
);

impl<T: Send + Sync + Serialize + DeserializeOwned + 'static> Responder for Form<T> {
    type Error = FormEncodeError;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        *response.body_mut() =
            http_kit::Body::from_form(&self.0).map_err(|_| FormEncodeError::new())?;
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
            schema: None,
            content_type: Some("application/x-www-form-urlencoded"),
        }])
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        _defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
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
    #[error("Failed to parse form data", status = StatusCode::BAD_REQUEST)]
    InvalidPayload,
}

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Form<T> {
    type Error = FormContentTypeError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        if request.method() == Method::GET {
            let data = request.uri().query().unwrap_or_default();
            extract(data)
        } else {
            if let Some(content_type) = request.headers().get(CONTENT_TYPE) {
                if !is_form_content_type(content_type) {
                    return Err(FormContentTypeError::Unsupported);
                }
            } else {
                return Err(FormContentTypeError::Missing);
            }

            let body = core::mem::replace(request.body_mut(), http_kit::Body::empty());
            let data = body
                .into_string()
                .await
                .map_err(|_| FormContentTypeError::InvalidPayload)?;
            extract(&data)
        }
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            content_type: Some("application/x-www-form-urlencoded"),
            schema: None,
        })
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        _defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
    }
}

fn extract<T: Send + Sync + DeserializeOwned>(data: &str) -> Result<Form<T>, FormContentTypeError> {
    from_str(data)
        .map(Form)
        .map_err(|_| FormContentTypeError::InvalidPayload)
}

impl_deref!(Form);

fn is_form_content_type(value: &HeaderValue) -> bool {
    value
        .to_str()
        .ok()
        .and_then(|raw| raw.split(';').next())
        .map(|mime| {
            mime.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{Form, FormContentTypeError};
    use crate::{Body, Method};
    use http_kit::{header::CONTENT_TYPE, Request};
    use serde::Deserialize;
    use skyzen_core::Extractor;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Payload {
        name: String,
        age: u8,
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
        assert!(matches!(error, FormContentTypeError::Missing));
    }

    #[tokio::test]
    async fn rejects_invalid_payload() {
        let mut request = request_with_body(b"name=Lexo&age=oops");
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let error = Form::<Payload>::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, FormContentTypeError::InvalidPayload));
    }

    #[tokio::test]
    async fn rejects_wrong_content_type() {
        let mut request = request_with_body(b"name=Lexo&age=17");
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("text/plain"),
        );
        let error = Form::<Payload>::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, FormContentTypeError::Unsupported));
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
}
