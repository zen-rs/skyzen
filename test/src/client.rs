//! HTTP test client for integration testing.

use http_kit::{
    header::{HeaderName, HeaderValue, CONTENT_TYPE},
    Body, Endpoint, HttpError, Method, Request, Response,
};
use serde::Serialize;

use crate::assertions::TestResponse;

/// An HTTP test client that sends requests to an endpoint without network I/O.
///
/// Created via [`TestContext::client`](crate::context::TestContext::client).
#[derive(Debug)]
pub struct TestClient<E> {
    endpoint: E,
}

impl<E: Endpoint + Clone> TestClient<E> {
    /// Create a new test client wrapping the given endpoint.
    pub(crate) const fn new(endpoint: E) -> Self {
        Self { endpoint }
    }

    /// Start building a GET request.
    #[must_use]
    pub fn get(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(self.endpoint.clone(), Method::GET, path)
    }

    /// Start building a POST request.
    #[must_use]
    pub fn post(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(self.endpoint.clone(), Method::POST, path)
    }

    /// Start building a PUT request.
    #[must_use]
    pub fn put(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(self.endpoint.clone(), Method::PUT, path)
    }

    /// Start building a PATCH request.
    #[must_use]
    pub fn patch(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(self.endpoint.clone(), Method::PATCH, path)
    }

    /// Start building a DELETE request.
    #[must_use]
    pub fn delete(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(self.endpoint.clone(), Method::DELETE, path)
    }
}

/// A builder for constructing and sending test HTTP requests.
#[derive(Debug)]
pub struct RequestBuilder<E> {
    endpoint: E,
    method: Method,
    uri: String,
    headers: Vec<(String, String)>,
    body: Body,
}

impl<E: Endpoint> RequestBuilder<E> {
    fn new(endpoint: E, method: Method, path: &str) -> Self {
        Self {
            endpoint,
            method,
            uri: path.to_owned(),
            headers: Vec::new(),
            body: Body::empty(),
        }
    }

    /// Add a header to the request.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Set the `Authorization: Bearer <token>` header.
    #[must_use]
    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", &format!("Bearer {token}"))
    }

    /// Set a JSON request body and the `Content-Type: application/json` header.
    ///
    /// # Panics
    ///
    /// Panics if the value cannot be serialized to JSON.
    #[must_use]
    pub fn json<T: Serialize>(mut self, value: &T) -> Self {
        let bytes = serde_json::to_vec(value).expect("failed to serialize request body to JSON");
        self.body = Body::from(bytes);
        self.headers
            .push(("Content-Type".to_owned(), "application/json".to_owned()));
        self
    }

    /// Set a raw body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// Send the request and return a testable response.
    ///
    /// # Panics
    ///
    /// Panics if the request URI is invalid or header construction fails.
    pub async fn send(self) -> TestResponse {
        let mut request = Request::new(self.body);
        *request.method_mut() = self.method;
        *request.uri_mut() = self.uri.parse().expect("invalid URI in test request");

        for (name, value) in &self.headers {
            request.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes())
                    .expect("invalid header name in test request"),
                HeaderValue::from_str(value).expect("invalid header value in test request"),
            );
        }

        let mut endpoint = self.endpoint;
        let response: Response = match endpoint.respond(&mut request).await {
            Ok(resp) => resp,
            Err(err) => {
                let status = err.status();
                let body = format!(
                    r#"{{"error":"{}"}}"#,
                    err.to_string().replace('\\', r"\\").replace('"', r#"\""#)
                );
                let mut resp = Response::new(Body::from(body));
                *resp.status_mut() = status;
                resp.headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                resp
            }
        };

        TestResponse::new(response).await
    }
}
