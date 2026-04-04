//! Test context providing HTTP client and utilities.

use http_kit::Endpoint;
use skyzen_services::{Db, Kv, Queue, Storage};

use crate::client::TestClient;

/// Test context providing HTTP client and test utilities.
///
/// `TestContext` is injected into test functions annotated with `#[skyzen::test]`.
/// When the macro also injects portable services such as [`Kv`](skyzen_services::Kv),
/// clients created from this context forward those services into every request automatically.
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
#[derive(Debug, Clone, Default)]
pub struct TestContext {
    kv: Option<Kv>,
    storage: Option<Storage>,
    queue: Option<Queue>,
    db: Option<Db>,
}

impl TestContext {
    /// Create a new test context.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            kv: None,
            storage: None,
            queue: None,
            db: None,
        }
    }

    /// Inject a KV service into every request sent by clients created from this context.
    #[must_use]
    pub fn with_kv(mut self, kv: Kv) -> Self {
        self.kv = Some(kv);
        self
    }

    /// Inject an object storage service into every request sent by this context's clients.
    #[must_use]
    pub fn with_storage(mut self, storage: Storage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Inject a queue service into every request sent by this context's clients.
    #[must_use]
    pub fn with_queue(mut self, queue: Queue) -> Self {
        self.queue = Some(queue);
        self
    }

    /// Inject a database service into every request sent by this context's clients.
    #[must_use]
    pub fn with_db(mut self, db: Db) -> Self {
        self.db = Some(db);
        self
    }

    /// Create an HTTP test client for the given endpoint.
    ///
    /// The client sends requests directly to the endpoint without network I/O.
    #[must_use]
    pub fn client<E: Endpoint + Clone>(&self, endpoint: E) -> TestClient<E> {
        TestClient::new(
            endpoint,
            self.kv.clone(),
            self.storage.clone(),
            self.queue.clone(),
            self.db.clone(),
        )
    }
}
