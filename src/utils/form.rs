//! Form utilities module.

use crate::{
    extract::Extractor,
    header::{HeaderValue, CONTENT_TYPE},
    responder::Responder,
    Request, Response, StatusCode,
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
    pub FormEncodeError, StatusCode::INTERNAL_SERVER_ERROR, "Failed to encode form data"
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
            schema: crate::openapi::maybe_schema_of::<T>(),
            content_type: Some("application/x-www-form-urlencoded"),
        }])
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
        crate::openapi::maybe_register_schema_for::<T>(defs);
    }
}

/// Error raised when a request cannot be extracted as `application/x-www-form-urlencoded`.
#[skyzen::error]
pub enum FormContentTypeError {
    /// The content type header is missing.
    #[error(
        "Expected content type `application/x-www-form-urlencoded`",
        status = StatusCode::BAD_REQUEST
    )]
    Missing,
    /// The content type header is not `application/x-www-form-urlencoded`.
    #[error(
        "Expected content type `application/x-www-form-urlencoded`",
        status = StatusCode::UNSUPPORTED_MEDIA_TYPE
    )]
    Unsupported,
    /// The request body could not be decoded as URL-encoded form data.
    #[error("Failed to parse form data", status = StatusCode::BAD_REQUEST)]
    InvalidPayload,
}

fn is_form_content_type(content_type: &HeaderValue) -> bool {
    let Ok(value) = content_type.to_str() else {
        return false;
    };

    let Some(media_type) = value.split(';').next().map(str::trim) else {
        return false;
    };

    media_type.eq_ignore_ascii_case("application/x-www-form-urlencoded")
}

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Form<T> {
    type Error = FormContentTypeError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
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

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            content_type: Some("application/x-www-form-urlencoded"),
            schema: crate::openapi::maybe_schema_of::<T>(),
        })
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
        crate::openapi::maybe_register_schema_for::<T>(defs);
    }
}

fn extract<T: Send + Sync + DeserializeOwned>(data: &str) -> Result<Form<T>, FormContentTypeError> {
    from_str(data).map(Form).map_err(|_| FormContentTypeError::InvalidPayload)
}

impl_deref!(Form);

#[cfg(test)]
mod tests {
    use super::{is_form_content_type, Form, FormContentTypeError};
    use crate::{Body, Method, StatusCode};
    use http_kit::{header::HeaderValue, HttpError};
    use serde::Deserialize;
    use skyzen_core::Extractor;

    #[derive(Debug, Deserialize, PartialEq)]
    struct LoginForm {
        user: String,
        remember: bool,
    }

    #[tokio::test]
    async fn extracts_form_body_when_content_type_matches() {
        let mut request = request(
            "user=lexo&remember=true",
            Some("application/x-www-form-urlencoded; charset=utf-8"),
        );
        let Form(form) = Form::<LoginForm>::extract(&mut request).await.unwrap();
        assert_eq!(
            form,
            LoginForm {
                user: "lexo".into(),
                remember: true,
            }
        );
    }

    #[tokio::test]
    async fn rejects_missing_content_type() {
        let mut request = request("user=lexo&remember=true", None);
        let error = Form::<LoginForm>::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, FormContentTypeError::Missing));
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_non_form_content_type() {
        let mut request = request("user=lexo&remember=true", Some("application/json"));
        let error = Form::<LoginForm>::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, FormContentTypeError::Unsupported));
        assert_eq!(error.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn rejects_invalid_payload() {
        let mut request = request("remember=not-a-bool", Some("application/x-www-form-urlencoded"));
        let error = Form::<LoginForm>::extract(&mut request).await.unwrap_err();
        assert!(matches!(error, FormContentTypeError::InvalidPayload));
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn accepts_form_content_type_with_parameters() {
        assert!(is_form_content_type(&HeaderValue::from_static(
            "application/x-www-form-urlencoded; charset=utf-8"
        )));
    }

    fn request(body: &str, content_type: Option<&str>) -> http_kit::Request {
        let mut request = http_kit::Request::new(Body::from(body.to_owned()));
        *request.method_mut() = Method::POST;
        if let Some(content_type) = content_type {
            request.headers_mut().insert(
                crate::header::CONTENT_TYPE,
                HeaderValue::from_str(content_type).expect("valid content type"),
            );
        }
        request
    }
}
