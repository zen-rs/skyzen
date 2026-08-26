use crate::{extract::Extractor, Request, StatusCode};

use serde::de::DeserializeOwned;
use serde_urlencoded::from_str;

/// Parse query from Uri.
#[derive(Debug, Clone)]
pub struct Query<T>(pub T);

impl_deref!(Query);

/// An error occurred while parsing the query string.
///
/// Carries the deserializer's own message so the 4xx response tells the caller which parameter
/// was wrong, rather than only saying that something was.
#[skyzen::error(
    message = "Failed to parse query string: {0}",
    status = StatusCode::BAD_REQUEST
)]
pub struct QueryError(String);

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Query<T> {
    type Error = QueryError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let data = request.uri().query().unwrap_or_default();
        Ok(Self(from_str(data).map_err(|error| {
            tracing::debug!(%error, "failed to parse query string");
            QueryError(error.to_string())
        })?))
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            location: crate::openapi::ParameterLocation::Query,
            content_type: None,
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

#[cfg(test)]
mod tests {
    use super::Query;
    use crate::{Body, Method, StatusCode};
    use http_kit::HttpError;
    use serde::Deserialize;
    use skyzen_core::Extractor;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Search {
        q: String,
        page: u8,
    }

    #[tokio::test]
    async fn parses_struct_from_query_string() {
        let mut request = request("http://localhost/search?q=rust&page=2");
        let Query(search) = Query::<Search>::extract(&mut request).await.unwrap();
        assert_eq!(
            search,
            Search {
                q: "rust".into(),
                page: 2
            }
        );
    }

    #[tokio::test]
    async fn surfaces_bad_request_for_invalid_payload() {
        let mut request = request("http://localhost/search?q=rust&page=two");
        let error = Query::<Search>::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let message = error.to_string();
        assert!(
            message.starts_with("Failed to parse query string: "),
            "rejection should keep its prefix, got {message}"
        );
        assert!(
            message.contains("invalid digit"),
            "rejection should carry the deserializer detail, got {message}"
        );
    }
    fn request(uri: &str) -> http_kit::Request {
        let mut request = http_kit::Request::new(Body::empty());
        *request.uri_mut() = uri.parse().expect("invalid uri");
        *request.method_mut() = Method::GET;
        request
    }
}
