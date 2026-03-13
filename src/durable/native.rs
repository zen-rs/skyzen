//! Native Durable Object simulator.

#![cfg(not(target_arch = "wasm32"))]

use std::{
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    marker::PhantomData,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
};

use futures_util::lock::Mutex;
use serde::Serialize;
use skyzen_services::{
    durable::{
        kv::{DurableKvStore, DurableListOptions},
        Alarm, AlarmError, AlarmScheduler, DurableDb, DurableDbBackend, DurableDbError, DurableKv,
        DurableKvError,
    },
    Db, DbError, DbExecResult, DbValue,
};

use super::{
    DurableConnections, DurableConnectionsInner, DurableObject, DurableObjectError,
    DurableObjectId, WebSocketConnection,
};
use crate::{Body, Endpoint, HttpError, Method, Request, Response};

const ALARM_REQUEST_PATH: &str = "/__skyzen_alarm";

/// Process-local namespace for simulating Durable Objects on native targets.
#[derive(Debug)]
pub struct NativeDurableNamespace<T> {
    inner: Arc<NativeDurableNamespaceInner>,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for NativeDurableNamespace<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            marker: PhantomData,
        }
    }
}

#[derive(Debug)]
struct NativeDurableNamespaceInner {
    type_name: &'static str,
    next_id: AtomicU64,
    instances: RwLock<HashMap<String, Arc<Mutex<NativeDurableSlot>>>>,
}

#[derive(Debug)]
struct NativeDurableSlot {
    state: Option<Vec<u8>>,
    kv: NativeDurableKvStore,
    db: NativeDurableDbStore,
    alarm: NativeAlarmScheduler,
    connections: NativeDurableConnections,
}

impl NativeDurableSlot {
    async fn new() -> Result<Self, DurableObjectError> {
        Ok(Self {
            state: None,
            kv: NativeDurableKvStore::default(),
            db: NativeDurableDbStore::new(
                Db::connect_sqlite_memory()
                    .await
                    .map_err(runtime_db_error)?,
            ),
            alarm: NativeAlarmScheduler::default(),
            connections: NativeDurableConnections,
        })
    }

    fn load_object<T>(&self) -> Result<T, DurableObjectError>
    where
        T: DurableObject,
    {
        self.state
            .as_deref()
            .map(|bytes| {
                serde_json::from_slice(bytes)
                    .map_err(|error| DurableObjectError::Serialization(error.to_string()))
            })
            .transpose()?
            .map_or_else(|| Ok(T::default()), Ok)
    }

    fn save_object<T>(&mut self, object: &T) -> Result<(), DurableObjectError>
    where
        T: DurableObject,
    {
        self.state = Some(
            serde_json::to_vec(object)
                .map_err(|error| DurableObjectError::Serialization(error.to_string()))?,
        );
        Ok(())
    }
}

impl<T> Default for NativeDurableNamespace<T>
where
    T: DurableObject,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> NativeDurableNamespace<T>
where
    T: DurableObject,
{
    /// Create a new in-process Durable Object namespace.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(NativeDurableNamespaceInner {
                type_name: std::any::type_name::<T>(),
                next_id: AtomicU64::new(1),
                instances: RwLock::new(HashMap::new()),
            }),
            marker: PhantomData,
        }
    }

    /// Resolve a deterministic Durable Object ID from a name.
    pub fn id_from_name(&self, name: &str) -> Result<DurableObjectId, DurableObjectError> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.inner.type_name.hash(&mut hasher);
        name.hash(&mut hasher);
        Ok(DurableObjectId::new(
            format!("native:{}:{:016x}", self.inner.type_name, hasher.finish()),
            Some(name.to_owned()),
        ))
    }

    /// Reconstruct a Durable Object ID from its string form.
    pub fn id_from_string(&self, id: &str) -> Result<DurableObjectId, DurableObjectError> {
        if id.trim().is_empty() {
            return Err(DurableObjectError::Runtime(
                "durable object id cannot be empty".to_owned(),
            ));
        }
        Ok(DurableObjectId::new(id.to_owned(), None))
    }

    /// Allocate a new unique Durable Object ID.
    pub fn new_unique_id(&self) -> Result<DurableObjectId, DurableObjectError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        Ok(DurableObjectId::new(
            format!("native:{}:{id}", self.inner.type_name),
            None,
        ))
    }

    /// Get a stub for the provided Durable Object ID.
    pub fn get(&self, id: &str) -> Result<NativeDurableObjectStub<T>, DurableObjectError> {
        Ok(NativeDurableObjectStub {
            namespace: self.clone(),
            id: self.id_from_string(id)?,
        })
    }

    /// Get a stub for a Durable Object by deterministic name.
    pub fn get_by_name(&self, name: &str) -> Result<NativeDurableObjectStub<T>, DurableObjectError> {
        Ok(NativeDurableObjectStub {
            namespace: self.clone(),
            id: self.id_from_name(name)?,
        })
    }

    async fn slot_for(
        &self,
        id: &DurableObjectId,
    ) -> Result<Arc<Mutex<NativeDurableSlot>>, DurableObjectError> {
        if let Some(slot) = self
            .inner
            .instances
            .read()
            .map_err(lock_poisoned)?
            .get(id.as_str())
            .cloned()
        {
            return Ok(slot);
        }

        let slot = Arc::new(Mutex::new(NativeDurableSlot::new().await?));
        let mut instances = self.inner.instances.write().map_err(lock_poisoned)?;
        Ok(instances
            .entry(id.as_str().to_owned())
            .or_insert_with(|| Arc::clone(&slot))
            .clone())
    }

    async fn fetch(
        &self,
        id: &DurableObjectId,
        mut request: Request,
    ) -> Result<Response, DurableObjectError> {
        let slot = self.slot_for(id).await?;
        let mut slot = slot.lock().await;
        let mut object = slot.load_object::<T>()?;

        inject_durable_extensions(&mut request, &slot, id.clone());

        let response = {
            let mut endpoint = object.fetch();
            match endpoint.respond(&mut request).await {
                Ok(response) => response,
                Err(error) => error_to_response(&error),
            }
        };

        slot.save_object(&object)?;
        Ok(response)
    }

    async fn alarm(&self, id: &DurableObjectId) -> Result<(), DurableObjectError> {
        let slot = self.slot_for(id).await?;
        let mut slot = slot.lock().await;
        let mut object = slot.load_object::<T>()?;

        let mut endpoint = object.fetch();
        let router = (&mut endpoint as &mut dyn std::any::Any)
            .downcast_mut::<crate::routing::Router>()
            .ok_or_else(|| {
                DurableObjectError::Runtime(
                    "DurableObject::fetch() must return skyzen::routing::Router for alarm handling"
                        .to_owned(),
                )
            })?;

        let mut alarm_endpoint = router.alarm_endpoint().ok_or_else(|| {
            DurableObjectError::Runtime(
                "No alarm handler registered. Use Route::on_alarm(handler).".to_owned(),
            )
        })?;

        let mut request = alarm_request().map_err(|error| DurableObjectError::Runtime(error.to_string()))?;
        inject_durable_extensions(&mut request, &slot, id.clone());

        alarm_endpoint
            .respond(&mut request)
            .await
            .map_err(|error| DurableObjectError::Runtime(error.to_string()))?;

        slot.save_object(&object)?;
        Ok(())
    }
}

/// Native stub for invoking a process-local Durable Object.
#[derive(Debug)]
pub struct NativeDurableObjectStub<T> {
    namespace: NativeDurableNamespace<T>,
    id: DurableObjectId,
}

impl<T> Clone for NativeDurableObjectStub<T> {
    fn clone(&self) -> Self {
        Self {
            namespace: self.namespace.clone(),
            id: self.id.clone(),
        }
    }
}

impl<T> NativeDurableObjectStub<T>
where
    T: DurableObject,
{
    /// The target Durable Object ID.
    #[must_use]
    pub const fn id(&self) -> &DurableObjectId {
        &self.id
    }

    /// Dispatch a request to the target Durable Object.
    pub async fn fetch(&self, request: Request) -> Result<Response, DurableObjectError> {
        self.namespace.fetch(&self.id, request).await
    }

    /// Dispatch a `GET` request to the target Durable Object using a URL string.
    pub async fn fetch_url(&self, url: &str) -> Result<Response, DurableObjectError> {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::GET;
        *request.uri_mut() = url
            .parse()
            .map_err(|error| DurableObjectError::Runtime(format!("invalid durable URL: {error}")))?;
        self.fetch(request).await
    }

    /// Trigger the target Durable Object's alarm handler.
    pub async fn alarm(&self) -> Result<(), DurableObjectError> {
        self.namespace.alarm(&self.id).await
    }
}

fn inject_durable_extensions(request: &mut Request, slot: &NativeDurableSlot, id: DurableObjectId) {
    request
        .extensions_mut()
        .insert(DurableKv::new(slot.kv.clone()));
    request
        .extensions_mut()
        .insert(DurableDb::new(slot.db.clone()));
    request.extensions_mut().insert(Alarm::new(slot.alarm.clone()));
    request.extensions_mut().insert(DurableConnections::new(Box::new(
        slot.connections.clone(),
    )));
    request.extensions_mut().insert(id);
}

fn alarm_request() -> Result<Request, http::Error> {
    let mut request = Request::new(Body::empty());
    *request.method_mut() = Method::POST;
    *request.uri_mut() = ALARM_REQUEST_PATH.parse()?;
    Ok(request)
}

fn error_to_response(error: &dyn HttpError) -> Response {
    let status = error.status();
    let message = error.to_string();

    let body = if status.is_server_error() {
        tracing::error!(status = status.as_u16(), error = %message, "durable fetch internal error");
        serde_json::json!({ "error": "Internal server error" }).to_string()
    } else {
        tracing::warn!(status = status.as_u16(), error = %message, "durable fetch client error");
        serde_json::json!({ "error": message }).to_string()
    };

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        crate::header::CONTENT_TYPE,
        crate::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn runtime_db_error(error: DbError) -> DurableObjectError {
    DurableObjectError::Runtime(error.to_string())
}

fn lock_poisoned<T>(_: T) -> DurableObjectError {
    DurableObjectError::Runtime("native durable simulator lock poisoned".to_owned())
}

#[derive(Debug, Clone, Default)]
struct NativeDurableConnections;

impl DurableConnectionsInner for NativeDurableConnections {
    fn all(&self) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        Ok(Vec::new())
    }

    fn by_tag(&self, _tag: &str) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        Ok(Vec::new())
    }

    fn set_auto_response(&self, _request: &str, _response: &str) -> Result<(), DurableObjectError> {
        Ok(())
    }

    fn clear_auto_response(&self) -> Result<(), DurableObjectError> {
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn DurableConnectionsInner> {
        Box::new(self.clone())
    }
}

#[derive(Debug, Clone, Default)]
struct NativeAlarmScheduler {
    alarm: Arc<RwLock<Option<i64>>>,
}

impl AlarmScheduler for NativeAlarmScheduler {
    async fn get_alarm(&self) -> Result<Option<i64>, AlarmError> {
        self.alarm
            .read()
            .map_err(|_| AlarmError::Backend("native durable alarm lock poisoned".to_owned()))
            .map(|value| *value)
    }

    async fn set_alarm(&self, scheduled_time_ms: i64) -> Result<(), AlarmError> {
        let mut alarm = self
            .alarm
            .write()
            .map_err(|_| AlarmError::Backend("native durable alarm lock poisoned".to_owned()))?;
        *alarm = Some(scheduled_time_ms);
        Ok(())
    }

    async fn delete_alarm(&self) -> Result<(), AlarmError> {
        let mut alarm = self
            .alarm
            .write()
            .map_err(|_| AlarmError::Backend("native durable alarm lock poisoned".to_owned()))?;
        *alarm = None;
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct NativeDurableKvStore {
    data: Arc<RwLock<BTreeMap<String, Vec<u8>>>>,
}

impl DurableKvStore for NativeDurableKvStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, DurableKvError> {
        let data = self.data.read().map_err(kv_lock_err)?;
        Ok(data.get(key).cloned())
    }

    async fn get_multiple(&self, keys: &[&str]) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
        let data = self.data.read().map_err(kv_lock_err)?;
        Ok(keys
            .iter()
            .filter_map(|key| data.get(*key).map(|value| ((*key).to_owned(), value.clone())))
            .collect())
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), DurableKvError> {
        self.data
            .write()
            .map_err(kv_lock_err)?
            .insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    async fn put_multiple(&self, entries: &[(&str, &[u8])]) -> Result<(), DurableKvError> {
        let mut guard = self.data.write().map_err(kv_lock_err)?;
        for (key, value) in entries {
            guard.insert((*key).to_owned(), value.to_vec());
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<bool, DurableKvError> {
        Ok(self.data.write().map_err(kv_lock_err)?.remove(key).is_some())
    }

    async fn delete_multiple(&self, keys: &[&str]) -> Result<usize, DurableKvError> {
        let mut guard = self.data.write().map_err(kv_lock_err)?;
        Ok(keys.iter().filter(|key| guard.remove(**key).is_some()).count())
    }

    async fn delete_all(&self) -> Result<(), DurableKvError> {
        self.data.write().map_err(kv_lock_err)?.clear();
        Ok(())
    }

    async fn list(
        &self,
        options: DurableListOptions<'_>,
    ) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
        let data = self.data.read().map_err(kv_lock_err)?;
        let iter: Box<dyn Iterator<Item = (&String, &Vec<u8>)>> = if options.reverse {
            Box::new(data.iter().rev())
        } else {
            Box::new(data.iter())
        };

        Ok(iter
            .filter(|(key, _)| {
                if let Some(prefix) = options.prefix {
                    if !key.starts_with(prefix) {
                        return false;
                    }
                }
                if let Some(start) = options.start {
                    if key.as_str() <= start {
                        return false;
                    }
                }
                if let Some(end) = options.end {
                    if key.as_str() >= end {
                        return false;
                    }
                }
                true
            })
            .take(options.limit.unwrap_or(usize::MAX))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }
}

fn kv_lock_err<T>(_: T) -> DurableKvError {
    DurableKvError::Backend("native durable KV lock poisoned".to_owned())
}

#[derive(Debug, Clone)]
struct NativeDurableDbStore {
    db: Db,
}

impl NativeDurableDbStore {
    const fn new(db: Db) -> Self {
        Self { db }
    }
}

impl DurableDbBackend for NativeDurableDbStore {
    async fn query(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DurableDbError> {
        let mut statement = self.db.query(query);
        for value in params {
            statement = statement.bind(value.clone());
        }
        let rows = statement
            .fetch_all::<serde_json::Value>()
            .await
            .map_err(durable_db_error)?;
        Ok(DbExecResult {
            rows_read: rows.len() as u64,
            rows,
            rows_written: 0,
        })
    }

    async fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> Result<DbExecResult, DurableDbError> {
        let mut statement = self.db.query(query);
        for value in params {
            statement = statement.bind(value.clone());
        }
        statement.execute().await.map_err(durable_db_error)
    }

    async fn database_size(&self) -> Result<u64, DurableDbError> {
        #[derive(serde::Deserialize)]
        struct PageCount {
            page_count: i64,
        }

        #[derive(serde::Deserialize)]
        struct PageSize {
            page_size: i64,
        }

        let page_count = self
            .db
            .query("PRAGMA page_count")
            .fetch_one::<PageCount>()
            .await
            .map_err(durable_db_error)?
            .page_count;
        let page_size = self
            .db
            .query("PRAGMA page_size")
            .fetch_one::<PageSize>()
            .await
            .map_err(durable_db_error)?
            .page_size;

        let bytes = page_count.checked_mul(page_size).ok_or_else(|| {
            DurableDbError::Backend("native durable DB size overflow".to_owned())
        })?;
        u64::try_from(bytes)
            .map_err(|_| DurableDbError::Backend("native durable DB size was negative".to_owned()))
    }
}

fn durable_db_error(error: DbError) -> DurableDbError {
    DurableDbError::Backend(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        routing::{CreateRouteNode, Route},
        Error, Result,
    };
    use serde::Deserialize;
    use skyzen_services::durable::DurableDb;

    #[derive(Default, Serialize, Deserialize)]
    #[skyzen::durable_object]
    struct CounterObject;

    impl DurableObject for CounterObject {
        fn fetch(&mut self) -> impl Endpoint + 'static {
            Route::new((
                "/increment".post(increment),
                "/value".at(value),
                "/alarm_count".at(alarm_count),
            ))
            .on_alarm(run_alarm)
            .build()
        }
    }

    async fn increment(db: DurableDb) -> Result<String> {
        db.query("CREATE TABLE IF NOT EXISTS counter (value INTEGER NOT NULL)")
            .execute()
            .await
            .map_err(to_error)?;

        #[derive(serde::Deserialize)]
        struct Row {
            value: i64,
        }

        let current = db
            .query("SELECT value FROM counter LIMIT 1")
            .fetch_optional::<Row>()
            .await
            .map_err(to_error)?
            .map_or(0, |row| row.value);
        let next = current + 1;

        if current == 0 {
            db.query("INSERT INTO counter (value) VALUES (?)")
                .bind(next)
                .execute()
                .await
                .map_err(to_error)?;
        } else {
            db.query("UPDATE counter SET value = ?")
                .bind(next)
                .execute()
                .await
                .map_err(to_error)?;
        }

        Ok(next.to_string())
    }

    async fn value(db: DurableDb) -> Result<String> {
        db.query("CREATE TABLE IF NOT EXISTS counter (value INTEGER NOT NULL)")
            .execute()
            .await
            .map_err(to_error)?;

        #[derive(serde::Deserialize)]
        struct Row {
            value: i64,
        }

        let current = db
            .query("SELECT value FROM counter LIMIT 1")
            .fetch_optional::<Row>()
            .await
            .map_err(to_error)?
            .map_or(0, |row| row.value);
        Ok(current.to_string())
    }

    async fn run_alarm(db: DurableDb) -> Result<&'static str> {
        db.query("CREATE TABLE IF NOT EXISTS alarm_runs (value INTEGER NOT NULL)")
            .execute()
            .await
            .map_err(to_error)?;

        #[derive(serde::Deserialize)]
        struct Row {
            value: i64,
        }

        let current = db
            .query("SELECT value FROM alarm_runs LIMIT 1")
            .fetch_optional::<Row>()
            .await
            .map_err(to_error)?
            .map_or(0, |row| row.value);
        let next = current + 1;

        if current == 0 {
            db.query("INSERT INTO alarm_runs (value) VALUES (?)")
                .bind(next)
                .execute()
                .await
                .map_err(to_error)?;
        } else {
            db.query("UPDATE alarm_runs SET value = ?")
                .bind(next)
                .execute()
                .await
                .map_err(to_error)?;
        }

        Ok("ok")
    }

    async fn alarm_count(db: DurableDb) -> Result<String> {
        db.query("CREATE TABLE IF NOT EXISTS alarm_runs (value INTEGER NOT NULL)")
            .execute()
            .await
            .map_err(to_error)?;

        #[derive(serde::Deserialize)]
        struct Row {
            value: i64,
        }

        let current = db
            .query("SELECT value FROM alarm_runs LIMIT 1")
            .fetch_optional::<Row>()
            .await
            .map_err(to_error)?
            .map_or(0, |row| row.value);
        Ok(current.to_string())
    }

    fn to_error(error: impl std::fmt::Display) -> Error {
        Error::msg(error.to_string())
    }

    fn request(method: Method, path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = method;
        *request.uri_mut() = path.parse().expect("valid durable test URI");
        request
    }

    async fn response_text(response: Response) -> String {
        let bytes = response
            .into_body()
            .into_bytes()
            .await
            .expect("response body");
        String::from_utf8(bytes.to_vec()).expect("utf8 body")
    }

    #[tokio::test]
    async fn native_durable_namespace_persists_db_per_object() {
        let namespace = NativeDurableNamespace::<CounterObject>::new();
        let object_a = namespace.get_by_name("a").expect("object a");
        let object_b = namespace.get_by_name("b").expect("object b");

        let first = object_a
            .fetch(request(Method::POST, "/increment"))
            .await
            .expect("increment a");
        let second = object_a
            .fetch(request(Method::POST, "/increment"))
            .await
            .expect("increment a again");
        let third = object_b
            .fetch(request(Method::POST, "/increment"))
            .await
            .expect("increment b");

        assert_eq!(response_text(first).await, "1");
        assert_eq!(response_text(second).await, "2");
        assert_eq!(response_text(third).await, "1");

        let value_a = object_a
            .fetch(request(Method::GET, "/value"))
            .await
            .expect("value a");
        let value_b = object_b
            .fetch(request(Method::GET, "/value"))
            .await
            .expect("value b");

        assert_eq!(response_text(value_a).await, "2");
        assert_eq!(response_text(value_b).await, "1");
    }

    #[tokio::test]
    async fn native_durable_namespace_runs_alarm_handler() {
        let namespace = NativeDurableNamespace::<CounterObject>::new();
        let stub = namespace.get_by_name("alarm").expect("alarm object");

        stub.alarm().await.expect("first alarm");
        stub.alarm().await.expect("second alarm");

        let response = stub
            .fetch(request(Method::GET, "/alarm_count"))
            .await
            .expect("alarm count");
        assert_eq!(response_text(response).await, "2");
    }
}
