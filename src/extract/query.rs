use crate::{extract::Extractor, Request, StatusCode};

use http_kit::http_error;
use serde::de::DeserializeOwned;
use serde_urlencoded::from_str;

/// Parse query from Uri.
#[derive(Debug, Clone)]
pub struct Query<T>(pub T);

impl_deref!(Query);

http_error!(
    /// An error occurred while parsing the query string.
    pub QueryError, StatusCode::BAD_REQUEST, "Failed to parse query string");

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Query<T> {
    type Error = QueryError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let data = request.uri().query().unwrap_or_default();
        Ok(Self(from_str(data).map_err(|_| QueryError::new())?))
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
        assert_eq!(error.status(), Some(StatusCode::BAD_REQUEST));
    }
    fn request(uri: &str) -> http_kit::Request {
        let mut request = http_kit::Request::new(Body::empty());
        *request.uri_mut() = uri.parse().expect("invalid uri");
        *request.method_mut() = Method::GET;
        request
    }
}
