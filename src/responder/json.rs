//! Json responder module.
//! It provides a responder serializing data as pretty-printed JSON.

use http_kit::{
    header::{HeaderValue, CONTENT_TYPE},
    Request, Response,
};
use http_kit::{http_error, StatusCode};
use serde::Serialize;
use serde_json::to_vec_pretty;
use skyzen_core::Responder;

/// A pretty JSON responder,it serialize data as a pretty-printed JSON.
/// # Example
/// ```
/// # use skyzen::responder::PrettyJson;
/// # use serde::Serialize;
/// #[derive(Serialize)]
/// struct User{
///     name:String,
///     age:u8
/// }
/// async fn handler() -> PrettyJson<User>{
///     PrettyJson(User{name:"Lexo".into(),age:17})
/// }
///
/// // Expected result:
/// //{
/// //  "name": "Lexo",
/// //  "age": 17
/// //}
///
/// ```
#[derive(Debug, Clone)]
pub struct PrettyJson<T: Send + Sync + Serialize>(pub T);

http_error!(
    /// An error occurred when serializing the JSON payload.
    pub PrettyJsonError, StatusCode::INTERNAL_SERVER_ERROR, "Failed to serialize JSON payload");

impl<T: Send + Sync + Serialize + 'static> Responder for PrettyJson<T> {
    type Error = PrettyJsonError;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        let payload = to_vec_pretty(&self.0).map_err(|error| {
            tracing::error!(%error, "failed to serialize JSON payload");
            PrettyJsonError::new()
        })?;
        *response.body_mut() = http_kit::Body::from_bytes(payload);
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<crate::openapi::ResponseSchema>> {
        Some(vec![crate::openapi::ResponseSchema {
            status: None,
            description: None,
            schema: crate::openapi::maybe_schema_of::<T>(),
            content_type: Some("application/json"),
        }])
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
        crate::openapi::maybe_register_schema_for::<T>(defs);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{header::CONTENT_TYPE, Body, Request, Response, ToSchema};
    use http_kit::HttpError;
    use serde::Serialize;
    use skyzen_core::Responder;

    use super::PrettyJson;

    #[derive(Debug, Serialize, ToSchema)]
    struct User {
        name: String,
        age: u8,
    }

    #[derive(Debug)]
    struct AlwaysFails;

    impl Serialize for AlwaysFails {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("expected serialization failure"))
        }
    }

    #[tokio::test]
    async fn writes_pretty_json_body_and_content_type() {
        let payload = User {
            name: "Lexo".to_owned(),
            age: 17,
        };
        let request = Request::new(Body::empty());
        let mut response = Response::new(Body::empty());

        PrettyJson(payload)
            .respond_to(&request, &mut response)
            .unwrap();

        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "{\n  \"name\": \"Lexo\",\n  \"age\": 17\n}");
    }

    #[test]
    fn surfaces_serialization_errors() {
        let request = Request::new(Body::empty());
        let mut response = Response::new(Body::empty());

        let error = PrettyJson(AlwaysFails)
            .respond_to(&request, &mut response)
            .unwrap_err();

        assert_eq!(error.status(), crate::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn openapi_includes_wrapped_schema_and_dependencies() {
        let schemas = <PrettyJson<User> as Responder>::openapi().unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].content_type, Some("application/json"));
        assert!(crate::openapi::schema_of::<User>().is_some());

        let mut defs = BTreeMap::new();
        <PrettyJson<User> as Responder>::register_openapi_schemas(&mut defs);
        let _ = defs;
    }
}
