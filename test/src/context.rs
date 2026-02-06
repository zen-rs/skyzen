//! Test context providing HTTP client and utilities.

use http_kit::Endpoint;

use crate::client::TestClient;

/// Test context providing HTTP client and test utilities.
///
/// `TestContext` is injected into test functions annotated with `#[skyzen::test]`.
/// Its primary purpose is to create HTTP test clients for integration testing.
///
/// # Example
///
/// ```ignore
/// #[skyzen::test]
/// async fn test_api(ctx: TestContext) {
///     let client = ctx.client(my_app());
///     let response = client.get("/users").send().await;
///     response.assert_status(200);
/// }
/// ```
#[derive(Debug)]
pub struct TestContext;

impl TestContext {
    /// Create a new test context.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Create an HTTP test client for the given endpoint.
    ///
    /// The client sends requests directly to the endpoint without network I/O.
    #[must_use]
    pub const fn client<E: Endpoint + Clone>(&self, endpoint: E) -> TestClient<E> {
        TestClient::new(endpoint)
    }
}

impl Default for TestContext {
    fn default() -> Self {
        Self::new()
    }
}
