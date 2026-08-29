//! HTTP test client for integration testing.

use http_kit::{
    header::{HeaderName, HeaderValue},
    Body, Endpoint, Method, Request, Response,
};
use serde::Serialize;

use crate::{assertions::TestResponse, context::InjectedServices};

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
    services: InjectedServices,
}

impl<E: Endpoint + Clone> TestClient<E> {
    /// Create a new test client wrapping the given endpoint.
    pub(crate) const fn new(endpoint: E, services: InjectedServices) -> Self {
        Self { endpoint, services }
    }

    /// Start building a request with any method.
    ///
    /// The verb helpers below delegate here; use it directly for a method they do not cover, or
    /// for one built at runtime.
    #[must_use]
    pub fn request(&self, method: Method, path: &str) -> RequestBuilder<E> {
        RequestBuilder::new(self.endpoint.clone(), method, path, self.services.clone())
    }

    /// Start building a GET request.
    #[must_use]
    pub fn get(&self, path: &str) -> RequestBuilder<E> {
        self.request(Method::GET, path)
    }

    /// Start building a POST request.
    #[must_use]
    pub fn post(&self, path: &str) -> RequestBuilder<E> {
        self.request(Method::POST, path)
    }

    /// Start building a PUT request.
    #[must_use]
    pub fn put(&self, path: &str) -> RequestBuilder<E> {
        self.request(Method::PUT, path)
    }

    /// Start building a PATCH request.
    #[must_use]
    pub fn patch(&self, path: &str) -> RequestBuilder<E> {
        self.request(Method::PATCH, path)
    }

    /// Start building a DELETE request.
    #[must_use]
    pub fn delete(&self, path: &str) -> RequestBuilder<E> {
        self.request(Method::DELETE, path)
    }

    /// Start building a HEAD request.
    #[must_use]
    pub fn head(&self, path: &str) -> RequestBuilder<E> {
        self.request(Method::HEAD, path)
    }

    /// Start building an OPTIONS request.
    ///
    /// This is what exercises a CORS preflight, which router-wide layers answer.
    #[must_use]
    pub fn options(&self, path: &str) -> RequestBuilder<E> {
        self.request(Method::OPTIONS, path)
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
    services: InjectedServices,
}

impl<E: Endpoint> RequestBuilder<E> {
    fn new(endpoint: E, method: Method, path: &str, services: InjectedServices) -> Self {
        Self {
            endpoint,
            method,
            uri: path.to_owned(),
            headers: Vec::new(),
            body: Body::empty(),
            services,
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
        let encoded = serde_html_form::to_string(value)
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

        self.services.install(&mut request);

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
    use core::future::{ready, Future};
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

        fn respond(
            &mut self,
            _request: &mut Request,
        ) -> impl Future<Output = Result<Response, Self::Error>> + Send {
            ready(Err(BackendBroken::new()))
        }
    }

    #[derive(Debug, Clone)]
    struct ClientErrorEndpoint;

    impl Endpoint for ClientErrorEndpoint {
        type Error = BadInput;

        fn respond(
            &mut self,
            _request: &mut Request,
        ) -> impl Future<Output = Result<Response, Self::Error>> + Send {
            ready(Err(BadInput::new()))
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

    #[derive(Debug, Clone)]
    struct MethodEchoEndpoint;

    impl Endpoint for MethodEchoEndpoint {
        type Error = std::convert::Infallible;

        fn respond(
            &mut self,
            request: &mut Request,
        ) -> impl Future<Output = Result<Response, Self::Error>> + Send {
            ready(Ok(Response::new(Body::from(request.method().to_string()))))
        }
    }

    #[tokio::test]
    async fn head_and_options_reach_the_endpoint() {
        let client = TestContext::new().client(MethodEchoEndpoint);

        assert_eq!(client.head("/resource").send().await.body_text(), "HEAD");
        // A CORS preflight is the reason OPTIONS needs to be reachable at all.
        assert_eq!(
            client.options("/resource").send().await.body_text(),
            "OPTIONS"
        );
    }

    #[tokio::test]
    async fn request_builds_a_method_the_verb_helpers_do_not_cover() {
        let response = TestContext::new()
            .client(MethodEchoEndpoint)
            .request(http_kit::Method::TRACE, "/resource")
            .send()
            .await;

        response.assert_status(200);
        assert_eq!(response.body_text(), "TRACE");
    }

    #[tokio::test]
    async fn durable_services_ride_the_context_into_the_request() {
        use skyzen_services::durable::{Alarm, DurableDb, DurableKv};

        use crate::mock::{InMemoryAlarm, InMemoryDurableDb, InMemoryDurableKv};

        #[derive(Debug, Clone)]
        struct DurableEndpoint;

        impl Endpoint for DurableEndpoint {
            type Error = std::convert::Infallible;

            async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
                use skyzen_core::Extractor as _;

                let kv = DurableKv::extract(request)
                    .await
                    .expect("durable kv should be injected");
                let db = DurableDb::extract(request)
                    .await
                    .expect("durable db should be injected");
                let alarm = Alarm::extract(request)
                    .await
                    .expect("alarm should be injected");

                kv.put("visited", b"1").await.expect("durable kv put");
                db.query("CREATE TABLE visits (note TEXT NOT NULL)")
                    .execute()
                    .await
                    .expect("durable ddl");
                db.query("INSERT INTO visits (note) VALUES (?)")
                    .bind("first")
                    .execute()
                    .await
                    .expect("durable insert");
                alarm.set_alarm(1337).await.expect("alarm set");

                Ok(Response::new(Body::from("ok")))
            }
        }

        let durable_kv = InMemoryDurableKv::new();
        let durable_db = InMemoryDurableDb::in_memory()
            .await
            .expect("in-memory SQLite should initialize");
        let alarm = InMemoryAlarm::new();

        let response = TestContext::new()
            .with_durable_kv(DurableKv::new(durable_kv.clone()))
            .with_durable_db(DurableDb::new(durable_db.clone()))
            .with_alarm(Alarm::new(alarm.clone()))
            .client(DurableEndpoint)
            .get("/durable")
            .send()
            .await;

        response.assert_status(200);
        assert_eq!(
            skyzen_services::durable::DurableKvStore::get(&durable_kv, "visited")
                .await
                .unwrap(),
            Some(b"1".to_vec())
        );
        // The durable database executes SQL rather than recording it, so the assertion is a real
        // read of what the handler wrote — the same check the code would make on Workers.
        let notes: Vec<String> = DurableDb::new(durable_db)
            .query("SELECT note FROM visits")
            .fetch_scalars()
            .await
            .expect("durable read-back");
        assert_eq!(notes, vec!["first".to_owned()]);
        assert_eq!(alarm.scheduled_time(), Some(1337));
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
