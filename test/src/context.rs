//! Test context providing HTTP client and utilities.

use http_kit::{Endpoint, Request};
use skyzen_services::{
    durable::{Alarm, DurableDb, DurableKv},
    Db, Kv, Queue, Storage,
};

use crate::client::TestClient;

/// The services a test injects into every request it sends.
///
/// One value carries all seven slots so [`TestContext`], [`TestClient`] and
/// [`RequestBuilder`](crate::client::RequestBuilder) share a single definition instead of
/// repeating the field list — and so adding a service is one field rather than three plus a
/// widening constructor.
#[derive(Debug, Clone, Default)]
pub(crate) struct InjectedServices {
    pub(crate) kv: Option<Kv>,
    pub(crate) storage: Option<Storage>,
    pub(crate) queue: Option<Queue>,
    pub(crate) db: Option<Db>,
    pub(crate) durable_kv: Option<DurableKv>,
    pub(crate) durable_db: Option<DurableDb>,
    pub(crate) alarm: Option<Alarm>,
}

impl InjectedServices {
    /// Put every configured service into the request's extensions.
    ///
    /// This is the same insertion `#[skyzen::main]` performs through service middleware, so a
    /// handler extracts them exactly as it would in production.
    pub(crate) fn install(&self, request: &mut Request) {
        let extensions = request.extensions_mut();
        if let Some(kv) = &self.kv {
            extensions.insert(kv.clone());
        }
        if let Some(storage) = &self.storage {
            extensions.insert(storage.clone());
        }
        if let Some(queue) = &self.queue {
            extensions.insert(queue.clone());
        }
        if let Some(db) = &self.db {
            extensions.insert(db.clone());
        }
        if let Some(durable_kv) = &self.durable_kv {
            extensions.insert(durable_kv.clone());
        }
        if let Some(durable_db) = &self.durable_db {
            extensions.insert(durable_db.clone());
        }
        if let Some(alarm) = &self.alarm {
            extensions.insert(alarm.clone());
        }
    }
}

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
    services: InjectedServices,
}

impl TestContext {
    /// Create a new test context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inject a KV service into every request sent by clients created from this context.
    #[must_use]
    pub fn with_kv(mut self, kv: Kv) -> Self {
        self.services.kv = Some(kv);
        self
    }

    /// Inject an object storage service into every request sent by this context's clients.
    #[must_use]
    pub fn with_storage(mut self, storage: Storage) -> Self {
        self.services.storage = Some(storage);
        self
    }

    /// Inject a queue service into every request sent by this context's clients.
    #[must_use]
    pub fn with_queue(mut self, queue: Queue) -> Self {
        self.services.queue = Some(queue);
        self
    }

    /// Inject a database service into every request sent by this context's clients.
    #[must_use]
    pub fn with_db(mut self, db: Db) -> Self {
        self.services.db = Some(db);
        self
    }

    /// Inject a Durable Object KV store into every request sent by this context's clients.
    #[must_use]
    pub fn with_durable_kv(mut self, durable_kv: DurableKv) -> Self {
        self.services.durable_kv = Some(durable_kv);
        self
    }

    /// Inject a Durable Object database into every request sent by this context's clients.
    #[must_use]
    pub fn with_durable_db(mut self, durable_db: DurableDb) -> Self {
        self.services.durable_db = Some(durable_db);
        self
    }

    /// Inject a Durable Object alarm scheduler into every request sent by this context's clients.
    #[must_use]
    pub fn with_alarm(mut self, alarm: Alarm) -> Self {
        self.services.alarm = Some(alarm);
        self
    }

    /// Create an HTTP test client for the given endpoint.
    ///
    /// The client sends requests directly to the endpoint without network I/O.
    #[must_use]
    pub fn client<E: Endpoint + Clone>(&self, endpoint: E) -> TestClient<E> {
        TestClient::new(endpoint, self.services.clone())
    }
}
