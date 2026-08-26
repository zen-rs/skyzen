//! HTTP test client for integration testing.

use http_kit::{
    header::{HeaderName, HeaderValue},
    Body, Endpoint, Method, Request, Response,
};
use serde::Serialize;
use skyzen_services::{Db, Kv, Queue, Storage};

use crate::assertions::TestResponse;

/// How a buffered header interacts with previously set values of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderMode {
    /// Replace any existing values (last write wins).
    Set,
    /// Add an additional value, keeping existing ones (multi-value headers).
    Append,
}

/// An HTTP test client that sends requests to an endpoint without network I/O.
///
/// Created via [`TestContext::client`](crate::context::TestContext::client).
#[derive(Debug)]
pub struct TestClient<E> {
    endpoint: E,
    kv: Option<Kv>,
    storage: Option<Storage>,
    queue: Option<Queue>,
    db: Option<Db>,
}

impl<E: Endpoint + Clone> TestClient<E> {
    /// Create a new test client wrapping the given endpoint.
    pub(crate) const fn new(
        endpoint: E,
        kv: Option<Kv>,
        storage: Option<Storage>,
        queue: Option<Queue>,
        db: Option<Db>,
    ) -> Self {
        Self {
            endpoint,
            kv,
            storage,
            queue,
            db,
        }
    }

    /// Start building a GET request.
    #[must_use]
    pub fn get(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(
            self.endpoint.clone(),
            Method::GET,
            path,
            self.kv.clone(),
            self.storage.clone(),
            self.queue.clone(),
            self.db.clone(),
        )
    }

    /// Start building a POST request.
    #[must_use]
    pub fn post(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(
            self.endpoint.clone(),
            Method::POST,
            path,
            self.kv.clone(),
            self.storage.clone(),
            self.queue.clone(),
            self.db.clone(),
        )
    }

    /// Start building a PUT request.
    #[must_use]
    pub fn put(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(
            self.endpoint.clone(),
            Method::PUT,
            path,
            self.kv.clone(),
            self.storage.clone(),
            self.queue.clone(),
            self.db.clone(),
        )
    }

    /// Start building a PATCH request.
    #[must_use]
    pub fn patch(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(
            self.endpoint.clone(),
            Method::PATCH,
            path,
            self.kv.clone(),
            self.storage.clone(),
            self.queue.clone(),
            self.db.clone(),
        )
    }

    /// Start building a DELETE request.
    #[must_use]
    pub fn delete(&self, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(
            self.endpoint.clone(),
            Method::DELETE,
            path,
            self.kv.clone(),
            self.storage.clone(),
            self.queue.clone(),
            self.db.clone(),
        )
    }
}

/// A builder for constructing and sending test HTTP requests.
#[derive(Debug)]
pub struct RequestBuilder<E> {
    endpoint: E,
    method: Method,
    uri: String,
    headers: Vec<(String, String, HeaderMode)>,
    body: Body,
    kv: Option<Kv>,
    storage: Option<Storage>,
    queue: Option<Queue>,
    db: Option<Db>,
}

impl<E: Endpoint> RequestBuilder<E> {
    fn new(
        endpoint: E,
        method: Method,
        path: &str,
        kv: Option<Kv>,
        storage: Option<Storage>,
        queue: Option<Queue>,
        db: Option<Db>,
    ) -> Self {
        Self {
            endpoint,
            method,
            uri: path.to_owned(),
            headers: Vec::new(),
            body: Body::empty(),
            kv,
            storage,
            queue,
            db,
        }
    }

    /// Set a header on the request, replacing any previously set value.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .push((name.to_owned(), value.to_owned(), HeaderMode::Set));
        self
    }

    /// Append a header value, keeping any previously set values.
    ///
    /// Use this to construct multi-value headers:
    ///
    /// ```ignore
    /// client.get("/")
    ///     .append_header("Accept-Encoding", "gzip")
    ///     .append_header("Accept-Encoding", "br")
    ///     .send()
    ///     .await;
    /// ```
    #[must_use]
    pub fn append_header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .push((name.to_owned(), value.to_owned(), HeaderMode::Append));
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
        self.headers.push((
            "Content-Type".to_owned(),
            "application/json".to_owned(),
            HeaderMode::Set,
        ));
        self
    }

    /// Set a URL-encoded form request body and the
    /// `Content-Type: application/x-www-form-urlencoded` header.
    ///
    /// # Panics
    ///
    /// Panics if the value cannot be serialized as a URL-encoded form.
    #[must_use]
    pub fn form<T: Serialize>(mut self, value: &T) -> Self {
        let encoded = serde_urlencoded::to_string(value)
            .expect("failed to serialize request body as URL-encoded form");
        self.body = Body::from(encoded);
        self.headers.push((
            "Content-Type".to_owned(),
            "application/x-www-form-urlencoded".to_owned(),
            HeaderMode::Set,
        ));
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

        for (name, value, mode) in &self.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .expect("invalid header name in test request");
            let value = HeaderValue::from_str(value).expect("invalid header value in test request");
            match mode {
                HeaderMode::Set => {
                    request.headers_mut().insert(name, value);
                }
                HeaderMode::Append => {
                    request.headers_mut().append(name, value);
                }
            }
        }

        if let Some(kv) = &self.kv {
            request.extensions_mut().insert(kv.clone());
        }
        if let Some(storage) = &self.storage {
            request.extensions_mut().insert(storage.clone());
        }
        if let Some(queue) = &self.queue {
            request.extensions_mut().insert(queue.clone());
        }
        if let Some(db) = &self.db {
            request.extensions_mut().insert(db.clone());
        }

        let mut endpoint = self.endpoint;
        // Capture the request identity before `respond` takes the request mutably.
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        // Render endpoint errors exactly the way production server backends
        // do (see `skyzen_core::error_response`): 5xx bodies are redacted to
        // a generic message, everything else surfaces the error's display
        // message as JSON. The log line is the shared one too, so a test can
        // observe the same record production emits.
        let response: Response = match endpoint.respond(&mut request).await {
            Ok(resp) => resp,
            Err(err) => {
                skyzen_core::log_endpoint_error(&err, &method, path.as_str());
                skyzen_core::error_response(&err)
            }
        };

        TestResponse::new(response).await
    }
}

#[cfg(test)]
mod tests {
    use http_kit::{Body, Endpoint, Request, Response, StatusCode};

    use crate::context::TestContext;

    http_kit::http_error!(
        /// Server-side test error.
        pub BackendBroken,
        StatusCode::INTERNAL_SERVER_ERROR,
        "database credentials leaked in this message"
    );

    http_kit::http_error!(
        /// Client-side test error.
        pub BadInput,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the `name` field is required"
    );

    #[derive(Debug, Clone)]
    struct ServerErrorEndpoint;

    impl Endpoint for ServerErrorEndpoint {
        type Error = BackendBroken;

        async fn respond(&mut self, _request: &mut Request) -> Result<Response, Self::Error> {
            Err(BackendBroken::new())
        }
    }

    #[derive(Debug, Clone)]
    struct ClientErrorEndpoint;

    impl Endpoint for ClientErrorEndpoint {
        type Error = BadInput;

        async fn respond(&mut self, _request: &mut Request) -> Result<Response, Self::Error> {
            Err(BadInput::new())
        }
    }

    #[derive(Debug, Clone)]
    struct EchoEndpoint;

    impl Endpoint for EchoEndpoint {
        type Error = std::convert::Infallible;

        async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
            let tags: Vec<String> = request
                .headers()
                .get_all("x-tag")
                .iter()
                .map(|value| value.to_str().expect("header should be UTF-8").to_owned())
                .collect();
            let content_type = request
                .headers()
                .get("content-type")
                .map(|value| value.to_str().expect("header should be UTF-8").to_owned())
                .unwrap_or_default();
            let body = core::mem::replace(request.body_mut(), Body::empty())
                .into_string()
                .await
                .expect("body should be UTF-8");
            Ok(Response::new(Body::from(format!(
                "tags={};content_type={};body={body}",
                tags.join(","),
                content_type
            ))))
        }
    }

    #[tokio::test]
    async fn server_errors_are_redacted_like_production() {
        let response = TestContext::new()
            .client(ServerErrorEndpoint)
            .get("/boom")
            .send()
            .await;

        response.assert_status(500);
        assert_eq!(response.body_text(), r#"{"error":"Internal server error"}"#);
    }

    #[tokio::test]
    async fn client_errors_keep_their_message_like_production() {
        let response = TestContext::new()
            .client(ClientErrorEndpoint)
            .get("/bad")
            .send()
            .await;

        response.assert_status(422);
        assert_eq!(
            response.body_text(),
            r#"{"error":"the `name` field is required"}"#
        );
    }

    #[tokio::test]
    async fn append_header_builds_multi_value_headers() {
        let response = TestContext::new()
            .client(EchoEndpoint)
            .get("/echo")
            .append_header("x-tag", "one")
            .append_header("x-tag", "two")
            .send()
            .await;

        response.assert_status(200);
        response.assert_body_contains("tags=one,two;");
    }

    #[tokio::test]
    async fn header_replaces_previous_value() {
        let response = TestContext::new()
            .client(EchoEndpoint)
            .get("/echo")
            .header("x-tag", "first")
            .header("x-tag", "second")
            .send()
            .await;

        response.assert_status(200);
        response.assert_body_contains("tags=second;");
    }

    #[tokio::test]
    async fn form_sets_urlencoded_body_and_content_type() {
        #[derive(serde::Serialize)]
        struct Login {
            user: String,
            remember: bool,
        }

        let response = TestContext::new()
            .client(EchoEndpoint)
            .post("/login")
            .form(&Login {
                user: "amélie".to_owned(),
                remember: true,
            })
            .send()
            .await;

        response.assert_status(200);
        response.assert_body_contains("content_type=application/x-www-form-urlencoded;");
        response.assert_body_contains("body=user=am%C3%A9lie&remember=true");
    }
}
