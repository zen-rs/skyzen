//! Native Durable Object simulator.

#![cfg(not(target_arch = "wasm32"))]

use core::future::{ready, Future};
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
#[cfg(feature = "ws")]
use futures_util::{FutureExt as _, StreamExt as _};
use skyzen_services::durable::{
    kv::{DurableKvStore, DurableListOptions},
    Alarm, AlarmError, AlarmScheduler, DurableDb, DurableKv, DurableKvError, SqliteDurableDb,
};

use super::{
    DurableConnections, DurableConnectionsInner, DurableObject, DurableObjectError,
    DurableObjectId, WebSocketConnection,
};
#[cfg(feature = "ws")]
use super::{WebSocketConnectionInner, WebSocketEvent};
#[cfg(feature = "ws")]
use crate::{
    durable::websocket::NativeDurableObjectState,
    websocket::{WebSocket, WebSocketCloseFrame, WebSocketMessage},
};
use crate::{Body, Endpoint, Method, Request, Response};

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
    instances: RwLock<HashMap<String, Arc<NativeDurableInstance>>>,
}

#[derive(Debug)]
struct NativeDurableInstance {
    slot: Mutex<NativeDurableSlot>,
    /// Serializes handler dispatch per object id, upholding the serial-execution promise
    /// documented on [`DurableObject`].
    dispatch: Mutex<()>,
}

#[derive(Debug)]
struct NativeDurableSlot {
    state: Option<Vec<u8>>,
    kv: NativeDurableKvStore,
    db: SqliteDurableDb,
    alarm: NativeAlarmScheduler,
    connections: NativeDurableConnections,
}

impl NativeDurableSlot {
    async fn new() -> Result<Self, DurableObjectError> {
        Ok(Self {
            state: None,
            kv: NativeDurableKvStore::default(),
            db: SqliteDurableDb::in_memory()
                .await
                .map_err(DurableObjectError::from)?,
            alarm: NativeAlarmScheduler::default(),
            connections: NativeDurableConnections::default(),
        })
    }

    /// Restore the user's object, or produce a fresh one when the type opted out of
    /// framework-managed persistence.
    ///
    /// The simulator honours [`DurableObject::PERSIST`] for the same reason the Cloudflare runtime
    /// does: an object that stores its own state must behave identically on both, or a bug only
    /// shows up after deployment.
    fn load_object<T>(&self) -> Result<T, DurableObjectError>
    where
        T: DurableObject,
    {
        if !T::PERSIST {
            return Ok(T::default());
        }

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
        if !T::PERSIST {
            return Ok(());
        }

        self.state = Some(
            serde_json::to_vec(object)
                .map_err(|error| DurableObjectError::Serialization(error.to_string()))?,
        );
        Ok(())
    }
}

impl<T> Default for NativeDurableNamespace<T>
where
    T: DurableObject + Send,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> NativeDurableNamespace<T>
where
    T: DurableObject + Send,
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
    ///
    /// # Errors
    ///
    /// This native implementation is deterministic and currently infallible,
    /// preserving the shared runtime API shape.
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
    ///
    /// # Errors
    ///
    /// Returns an error when `id` is empty or only whitespace.
    pub fn id_from_string(&self, id: &str) -> Result<DurableObjectId, DurableObjectError> {
        if id.trim().is_empty() {
            return Err(DurableObjectError::Runtime(
                "durable object id cannot be empty".to_owned(),
            ));
        }
        Ok(DurableObjectId::new(id.to_owned(), None))
    }

    /// Allocate a new unique Durable Object ID.
    ///
    /// # Errors
    ///
    /// This native implementation is currently infallible, preserving the
    /// shared runtime API shape.
    pub fn new_unique_id(&self) -> Result<DurableObjectId, DurableObjectError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        Ok(DurableObjectId::new(
            format!("native:{}:{id}", self.inner.type_name),
            None,
        ))
    }

    /// Get a stub for the provided Durable Object ID.
    ///
    /// # Errors
    ///
    /// Returns an error when `id` is empty or only whitespace.
    pub fn get(&self, id: &str) -> Result<NativeDurableObjectStub<T>, DurableObjectError> {
        Ok(NativeDurableObjectStub {
            namespace: self.clone(),
            id: self.id_from_string(id)?,
        })
    }

    /// Get a stub for a Durable Object by deterministic name.
    ///
    /// # Errors
    ///
    /// This native implementation is currently infallible, preserving the
    /// shared runtime API shape.
    pub fn get_by_name(
        &self,
        name: &str,
    ) -> Result<NativeDurableObjectStub<T>, DurableObjectError> {
        Ok(NativeDurableObjectStub {
            namespace: self.clone(),
            id: self.id_from_name(name)?,
        })
    }

    async fn slot_for(
        &self,
        id: &DurableObjectId,
    ) -> Result<Arc<NativeDurableInstance>, DurableObjectError> {
        let existing_instance = {
            self.inner
                .instances
                .read()
                .map_err(lock_poisoned)?
                .get(id.as_str())
                .cloned()
        };
        if let Some(instance) = existing_instance {
            return Ok(instance);
        }

        let slot = NativeDurableSlot::new().await?;
        self.install_alarm_timer(&slot.alarm, id);
        let instance = Arc::new(NativeDurableInstance {
            slot: Mutex::new(slot),
            dispatch: Mutex::new(()),
        });
        let instance = {
            let mut instances = self.inner.instances.write().map_err(lock_poisoned)?;
            instances
                .entry(id.as_str().to_owned())
                .or_insert_with(|| Arc::clone(&instance))
                .clone()
        };
        Ok(instance)
    }

    /// Wire the per-object alarm scheduler to a background timer that invokes the object's
    /// alarm handler when the scheduled time is reached, mirroring platform behavior.
    fn install_alarm_timer(&self, scheduler: &NativeAlarmScheduler, id: &DurableObjectId) {
        let weak = Arc::downgrade(&self.inner);
        let object_id = id.clone();
        let alarm_state = Arc::clone(&scheduler.alarm);
        let generation = Arc::clone(&scheduler.generation);

        scheduler.install_fire_handler(Box::new(move |scheduled_time_ms, expected_generation| {
            let weak = weak.clone();
            let object_id = object_id.clone();
            let alarm_state = Arc::clone(&alarm_state);
            let generation = Arc::clone(&generation);

            std::thread::spawn(move || {
                let now = current_unix_ms();
                if scheduled_time_ms > now {
                    let delta = u64::try_from(scheduled_time_ms - now).unwrap_or(0);
                    std::thread::sleep(std::time::Duration::from_millis(delta));
                }

                // A newer `set_alarm`/`delete_alarm` supersedes this timer.
                if generation.load(Ordering::SeqCst) != expected_generation {
                    return;
                }
                // Clear the stored alarm before dispatch (platform semantics: a fired alarm
                // no longer shows up via `get_alarm`).
                {
                    let Ok(mut stored) = alarm_state.write() else {
                        return;
                    };
                    if stored.take().is_none() {
                        return;
                    }
                }

                let Some(inner) = weak.upgrade() else { return };
                let namespace = Self {
                    inner,
                    marker: PhantomData,
                };
                if let Err(error) = smol::block_on(namespace.alarm(&object_id)) {
                    tracing::error!(%error, "native durable alarm handler failed");
                }
            });
        }));
    }

    async fn fetch(
        &self,
        id: &DurableObjectId,
        mut request: Request,
    ) -> Result<Response, DurableObjectError> {
        let instance = self.slot_for(id).await?;

        // Serialize the whole load → dispatch → save sequence per object id, upholding the
        // serial-execution model documented on `DurableObject`. Note that a handler which
        // re-enters the same object id through its own stub will deadlock, mirroring the
        // platform's serial input semantics.
        let _dispatch = instance.dispatch.lock().await;

        let mut object = {
            let slot = instance.slot.lock().await;
            inject_durable_extensions(&mut request, &slot, id.clone());
            #[cfg(feature = "ws")]
            {
                let namespace = self.clone();
                let object_id = id.clone();
                request
                    .extensions_mut()
                    .insert(NativeDurableObjectState::new(move |websocket, tags| {
                        let namespace = namespace.clone();
                        let object_id = object_id.clone();
                        async move {
                            if let Err(error) =
                                namespace.run_websocket(&object_id, websocket, tags).await
                            {
                                tracing::error!(%error, "native durable websocket session failed");
                            }
                        }
                    }));
            }
            slot.load_object::<T>()?
        };

        // Capture the request identity before `respond` takes the request mutably, so the error
        // log can name the call that failed the way the HTTP backends do.
        let method = request.method().clone();
        let path = request.uri().path().to_owned();

        let response = {
            let mut endpoint = object.fetch();
            match endpoint.respond(&mut request).await {
                Ok(response) => response,
                Err(error) => {
                    skyzen_core::log_endpoint_error(&error, &method, path.as_str());
                    skyzen_core::error_response(&error)
                }
            }
        };

        instance.slot.lock().await.save_object(&object)?;
        Ok(response)
    }

    async fn alarm(&self, id: &DurableObjectId) -> Result<(), DurableObjectError> {
        let instance = self.slot_for(id).await?;

        // See `fetch`: alarm dispatch participates in the same per-object serialization.
        let _dispatch = instance.dispatch.lock().await;

        let guard = instance.slot.lock().await;
        let mut object = guard.load_object::<T>()?;

        // `fetch()` returns a `Router`, which exposes the alarm handler registered via
        // `Route::on_alarm` directly — no runtime downcast required.
        let mut alarm_endpoint = object.fetch().alarm_endpoint().ok_or_else(|| {
            DurableObjectError::Runtime(
                "No alarm handler registered. Use Route::on_alarm(handler).".to_owned(),
            )
        })?;

        let mut request =
            alarm_request().map_err(|error| DurableObjectError::Runtime(error.to_string()))?;
        inject_durable_extensions(&mut request, &guard, id.clone());
        drop(guard);

        alarm_endpoint
            .respond(&mut request)
            .await
            .map_err(|error| DurableObjectError::Runtime(error.to_string()))?;

        // Persist any state the alarm handler wrote through the injected services.
        instance.slot.lock().await.save_object(&object)?;
        Ok(())
    }

    #[cfg(feature = "ws")]
    async fn dispatch_websocket_event(
        &self,
        id: &DurableObjectId,
        websocket: &WebSocketConnection,
        event: WebSocketEvent,
    ) -> Result<(), DurableObjectError> {
        let instance = self.slot_for(id).await?;
        let _dispatch = instance.dispatch.lock().await;

        let (mut object, context) = {
            let slot = instance.slot.lock().await;
            (
                slot.load_object::<T>()?,
                super::DurableContext::new(
                    DurableKv::new(slot.kv.clone()),
                    DurableDb::new(slot.db.clone()),
                    Alarm::new(slot.alarm.clone()),
                    DurableConnections::new(Box::new(slot.connections.clone())),
                    id.clone(),
                ),
            )
        };

        object.websocket(websocket, event, &context).await?;
        instance.slot.lock().await.save_object(&object)?;
        Ok(())
    }

    #[cfg(feature = "ws")]
    async fn run_websocket(
        &self,
        id: &DurableObjectId,
        websocket: WebSocket,
        tags: Vec<String>,
    ) -> Result<(), DurableObjectError> {
        let instance = self.slot_for(id).await?;
        let (connection_id, inner, mut commands) = {
            let slot = instance.slot.lock().await;
            slot.connections.register(tags)?
        };
        let connection = WebSocketConnection::new(Box::new(inner));
        let (mut sender, mut receiver) = websocket.split();

        let outcome = async {
            loop {
                futures_util::select! {
                    incoming = receiver.next().fuse() => {
                        match incoming {
                            Some(Ok(WebSocketMessage::Text(text))) => {
                                let auto_response = {
                                    let slot = instance.slot.lock().await;
                                    slot.connections.auto_response(text.as_str())?
                                };
                                if let Some(response) = auto_response {
                                    sender.send_text(response).await?;
                                } else {
                                    self.dispatch_websocket_event(
                                        id,
                                        &connection,
                                        WebSocketEvent::Message(WebSocketMessage::Text(text)),
                                    ).await?;
                                }
                            }
                            Some(Ok(WebSocketMessage::Binary(data))) => {
                                self.dispatch_websocket_event(
                                    id,
                                    &connection,
                                    WebSocketEvent::Message(WebSocketMessage::Binary(data)),
                                ).await?;
                            }
                            Some(Ok(WebSocketMessage::Ping(data))) => {
                                sender.send_pong(data).await?;
                            }
                            Some(Ok(WebSocketMessage::Pong(_))) => {}
                            Some(Ok(WebSocketMessage::Close)) | None => {
                                self.dispatch_websocket_event(
                                    id,
                                    &connection,
                                    WebSocketEvent::Close {
                                        code: 1000,
                                        reason: String::new(),
                                        was_clean: true,
                                    },
                                ).await?;
                                break;
                            }
                            Some(Err(error)) => {
                                self.dispatch_websocket_event(
                                    id,
                                    &connection,
                                    WebSocketEvent::Error(error.to_string()),
                                ).await?;
                                break;
                            }
                        }
                    },
                    command = commands.next().fuse() => {
                        match command {
                            Some(NativeWebSocketCommand::Text(text)) => {
                                sender.send_text(text).await?;
                            }
                            Some(NativeWebSocketCommand::Binary(data)) => {
                                sender.send_binary(data).await?;
                            }
                            Some(NativeWebSocketCommand::Close { code, reason }) => {
                                sender
                                    .close(Some(WebSocketCloseFrame::new(code, reason)))
                                    .await
                                    ?;
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }
            Ok(())
        }
        .await;

        let removal = instance.slot.lock().await.connections.remove(connection_id);
        outcome.and(removal)
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
    T: DurableObject + Send,
{
    /// The target Durable Object ID.
    #[must_use]
    pub const fn id(&self) -> &DurableObjectId {
        &self.id
    }

    /// Dispatch a request to the target Durable Object.
    ///
    /// # Errors
    ///
    /// Returns an error if object loading, request dispatch, or state
    /// persistence fails.
    pub async fn fetch(&self, request: Request) -> Result<Response, DurableObjectError> {
        self.namespace.fetch(&self.id, request).await
    }

    /// Dispatch a `GET` request to the target Durable Object using a URL string.
    ///
    /// # Errors
    ///
    /// Returns an error if `url` is invalid or the object fetch fails.
    pub async fn fetch_url(&self, url: &str) -> Result<Response, DurableObjectError> {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::GET;
        *request.uri_mut() = url.parse().map_err(|error| {
            DurableObjectError::Runtime(format!("invalid durable URL: {error}"))
        })?;
        self.fetch(request).await
    }

    /// Trigger the target Durable Object's alarm handler.
    ///
    /// # Errors
    ///
    /// Returns an error if object loading, alarm dispatch, or state
    /// persistence fails.
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
    request
        .extensions_mut()
        .insert(Alarm::new(slot.alarm.clone()));
    request
        .extensions_mut()
        .insert(DurableConnections::new(Box::new(slot.connections.clone())));
    request.extensions_mut().insert(id);
}

fn alarm_request() -> Result<Request, http::Error> {
    let mut request = Request::new(Body::empty());
    *request.method_mut() = Method::POST;
    *request.uri_mut() = ALARM_REQUEST_PATH.parse()?;
    Ok(request)
}

fn lock_poisoned<T>(_: T) -> DurableObjectError {
    DurableObjectError::Runtime("native durable simulator lock poisoned".to_owned())
}

/// The sockets one simulated object has accepted.
///
/// Shared by handle: every clone of the object's slot, and every [`WebSocketConnection`] handed to
/// a handler, has to see the same registry.
#[cfg(feature = "ws")]
#[derive(Debug, Clone, Default)]
struct NativeDurableConnections {
    state: Arc<NativeDurableConnectionsState>,
}

#[cfg(feature = "ws")]
#[derive(Debug, Default)]
struct NativeDurableConnectionsState {
    next_id: AtomicU64,
    sockets: RwLock<HashMap<u64, NativeWebSocketInner>>,
    auto_response: RwLock<Option<(String, String)>>,
}

/// Without the `ws` feature nothing can accept a socket, so there is no registry to keep.
#[cfg(not(feature = "ws"))]
#[derive(Debug, Clone, Default)]
struct NativeDurableConnections {}

#[cfg(feature = "ws")]
#[derive(Debug)]
enum NativeWebSocketCommand {
    Text(String),
    Binary(Vec<u8>),
    Close { code: u16, reason: String },
}

#[cfg(feature = "ws")]
#[derive(Debug, Clone)]
struct NativeWebSocketInner {
    commands: futures_channel::mpsc::UnboundedSender<NativeWebSocketCommand>,
    tags: Arc<Vec<String>>,
    attachment: Arc<RwLock<Option<Vec<u8>>>>,
}

#[cfg(feature = "ws")]
impl WebSocketConnectionInner for NativeWebSocketInner {
    fn send_text(&self, text: &str) -> Result<(), DurableObjectError> {
        self.commands
            .unbounded_send(NativeWebSocketCommand::Text(text.to_owned()))
            .map_err(|_| closed_websocket())
    }

    fn send_binary(&self, data: &[u8]) -> Result<(), DurableObjectError> {
        self.commands
            .unbounded_send(NativeWebSocketCommand::Binary(data.to_vec()))
            .map_err(|_| closed_websocket())
    }

    fn close(&self, code: u16, reason: &str) -> Result<(), DurableObjectError> {
        self.commands
            .unbounded_send(NativeWebSocketCommand::Close {
                code,
                reason: reason.to_owned(),
            })
            .map_err(|_| closed_websocket())
    }

    fn tags(&self) -> Result<Vec<String>, DurableObjectError> {
        Ok(self.tags.as_ref().clone())
    }

    fn get_attachment_raw(&self) -> Result<Option<Vec<u8>>, DurableObjectError> {
        self.attachment
            .read()
            .map_err(lock_poisoned)
            .map(|attachment| attachment.clone())
    }

    fn set_attachment_raw(&self, data: &[u8]) -> Result<(), DurableObjectError> {
        self.attachment
            .write()
            .map_err(lock_poisoned)
            .map(|mut attachment| *attachment = Some(data.to_vec()))
    }
}

#[cfg(feature = "ws")]
fn closed_websocket() -> DurableObjectError {
    DurableObjectError::WebSocket("native durable websocket is closed".to_owned())
}

#[cfg(feature = "ws")]
impl NativeDurableConnections {
    fn register(
        &self,
        tags: Vec<String>,
    ) -> Result<
        (
            u64,
            NativeWebSocketInner,
            futures_channel::mpsc::UnboundedReceiver<NativeWebSocketCommand>,
        ),
        DurableObjectError,
    > {
        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let (commands, receiver) = futures_channel::mpsc::unbounded();
        let inner = NativeWebSocketInner {
            commands,
            tags: Arc::new(tags),
            attachment: Arc::new(RwLock::new(None)),
        };
        self.state
            .sockets
            .write()
            .map_err(lock_poisoned)?
            .insert(id, inner.clone());
        Ok((id, inner, receiver))
    }

    fn remove(&self, id: u64) -> Result<(), DurableObjectError> {
        self.state
            .sockets
            .write()
            .map_err(lock_poisoned)?
            .remove(&id);
        Ok(())
    }

    fn auto_response(&self, message: &str) -> Result<Option<String>, DurableObjectError> {
        self.state
            .auto_response
            .read()
            .map_err(lock_poisoned)
            .map(|pair| {
                pair.as_ref()
                    .filter(|(request, _)| request == message)
                    .map(|(_, response)| response.clone())
            })
    }

    /// Every registered socket, or only those carrying `tag`.
    ///
    /// `all` and `by_tag` differ by exactly this filter, so they share one read of the registry.
    fn connections(
        &self,
        tag: Option<&str>,
    ) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        self.state
            .sockets
            .read()
            .map_err(lock_poisoned)
            .map(|sockets| {
                sockets
                    .values()
                    .filter(|socket| {
                        tag.is_none_or(|tag| socket.tags.iter().any(|candidate| candidate == tag))
                    })
                    .cloned()
                    .map(|socket| WebSocketConnection::new(Box::new(socket)))
                    .collect()
            })
    }

    /// Remember the auto-response pair, or forget it when given `None`.
    fn store_auto_response(&self, pair: Option<(&str, &str)>) -> Result<(), DurableObjectError> {
        self.state
            .auto_response
            .write()
            .map_err(lock_poisoned)
            .map(|mut slot| {
                *slot = pair.map(|(request, response)| (request.to_owned(), response.to_owned()));
            })
    }
}

/// Without the `ws` feature nothing can accept a socket, so an object has no connection to report
/// and no auto-response to remember.
#[cfg(not(feature = "ws"))]
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

#[cfg(feature = "ws")]
impl DurableConnectionsInner for NativeDurableConnections {
    fn all(&self) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        self.connections(None)
    }

    fn by_tag(&self, tag: &str) -> Result<Vec<WebSocketConnection>, DurableObjectError> {
        self.connections(Some(tag))
    }

    fn set_auto_response(&self, request: &str, response: &str) -> Result<(), DurableObjectError> {
        self.store_auto_response(Some((request, response)))
    }

    fn clear_auto_response(&self) -> Result<(), DurableObjectError> {
        self.store_auto_response(None)
    }

    fn clone_box(&self) -> Box<dyn DurableConnectionsInner> {
        Box::new(self.clone())
    }
}

/// Handler invoked when an alarm is scheduled; receives the scheduled unix time (ms) and the
/// schedule generation that must still be current when the timer elapses.
type AlarmFireHandler = Box<dyn Fn(i64, u64) + Send + Sync>;

#[derive(Clone, Default)]
struct NativeAlarmScheduler {
    alarm: Arc<RwLock<Option<i64>>>,
    /// Monotonic schedule counter; each `set_alarm`/`delete_alarm` bumps it so an in-flight
    /// timer can detect it has been superseded or cancelled.
    generation: Arc<AtomicU64>,
    fire: Arc<std::sync::OnceLock<AlarmFireHandler>>,
}

impl std::fmt::Debug for NativeAlarmScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeAlarmScheduler")
            .field("alarm", &self.alarm)
            .finish_non_exhaustive()
    }
}

impl NativeAlarmScheduler {
    fn install_fire_handler(&self, handler: AlarmFireHandler) {
        // Only the first installation wins; the handler is wired once per object slot.
        let _ = self.fire.set(handler);
    }
}

fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

impl NativeAlarmScheduler {
    fn store(&self, scheduled_time_ms: Option<i64>) -> Result<(), AlarmError> {
        self.alarm
            .write()
            .map_err(|_| AlarmError::backend("native durable alarm lock poisoned"))
            .map(|mut alarm| *alarm = scheduled_time_ms)
    }
}

// The alarm slot is a lock away, and arming the timer only hands work to a background task, so
// each future is ready on creation rather than an `async` block with nothing to await.
impl AlarmScheduler for NativeAlarmScheduler {
    fn get_alarm(&self) -> impl Future<Output = Result<Option<i64>, AlarmError>> + Send {
        ready(
            self.alarm
                .read()
                .map_err(|_| AlarmError::backend("native durable alarm lock poisoned"))
                .map(|alarm| *alarm),
        )
    }

    fn set_alarm(
        &self,
        scheduled_time_ms: i64,
    ) -> impl Future<Output = Result<(), AlarmError>> + Send {
        ready(self.store(Some(scheduled_time_ms)).map(|()| {
            let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            // Drive the alarm with a background timer so it actually fires in the simulator.
            if let Some(fire) = self.fire.get() {
                fire(scheduled_time_ms, generation);
            }
        }))
    }

    fn delete_alarm(&self) -> impl Future<Output = Result<(), AlarmError>> + Send {
        ready(self.store(None).map(|()| {
            // Invalidate any pending timer.
            self.generation.fetch_add(1, Ordering::SeqCst);
        }))
    }
}

#[derive(Debug, Clone, Default)]
struct NativeDurableKvStore {
    data: Arc<RwLock<BTreeMap<String, Vec<u8>>>>,
}

// A `BTreeMap` behind a lock answers every call synchronously, so each future is ready on
// creation rather than an `async` block with nothing to await.
impl DurableKvStore for NativeDurableKvStore {
    fn get(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, DurableKvError>> + Send {
        ready(
            self.data
                .read()
                .map_err(kv_lock_err)
                .map(|data| data.get(key).cloned()),
        )
    }

    fn get_multiple(
        &self,
        keys: &[&str],
    ) -> impl Future<Output = Result<Vec<(String, Vec<u8>)>, DurableKvError>> + Send {
        ready(self.data.read().map_err(kv_lock_err).map(|data| {
            keys.iter()
                .filter_map(|key| {
                    data.get(*key)
                        .map(|value| ((*key).to_owned(), value.clone()))
                })
                .collect()
        }))
    }

    fn put(
        &self,
        key: &str,
        value: &[u8],
    ) -> impl Future<Output = Result<(), DurableKvError>> + Send {
        ready(self.data.write().map_err(kv_lock_err).map(|mut guard| {
            guard.insert(key.to_owned(), value.to_vec());
        }))
    }

    fn put_multiple(
        &self,
        entries: &[(&str, &[u8])],
    ) -> impl Future<Output = Result<(), DurableKvError>> + Send {
        ready(self.data.write().map_err(kv_lock_err).map(|mut guard| {
            for (key, value) in entries {
                guard.insert((*key).to_owned(), value.to_vec());
            }
        }))
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<bool, DurableKvError>> + Send {
        ready(
            self.data
                .write()
                .map_err(kv_lock_err)
                .map(|mut guard| guard.remove(key).is_some()),
        )
    }

    fn delete_multiple(
        &self,
        keys: &[&str],
    ) -> impl Future<Output = Result<usize, DurableKvError>> + Send {
        ready(self.data.write().map_err(kv_lock_err).map(|mut guard| {
            keys.iter()
                .filter(|key| guard.remove(**key).is_some())
                .count()
        }))
    }

    fn delete_all(&self) -> impl Future<Output = Result<(), DurableKvError>> + Send {
        ready(
            self.data
                .write()
                .map_err(kv_lock_err)
                .map(|mut guard| guard.clear()),
        )
    }

    fn list(
        &self,
        options: DurableListOptions<'_>,
    ) -> impl Future<Output = Result<Vec<(String, Vec<u8>)>, DurableKvError>> + Send {
        ready(self.data.read().map_err(kv_lock_err).map(|data| {
            let iter: Box<dyn Iterator<Item = (&String, &Vec<u8>)>> = if options.reverse {
                Box::new(data.iter().rev())
            } else {
                Box::new(data.iter())
            };

            iter.filter(|(key, _)| {
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
            .collect()
        }))
    }
}

fn kv_lock_err<T>(_: T) -> DurableKvError {
    DurableKvError::backend("native durable KV lock poisoned")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        routing::{CreateRouteNode, Route},
        Result,
    };
    use serde::{Deserialize, Serialize};
    use skyzen_services::durable::DurableDb;

    #[derive(Default, Serialize, Deserialize)]
    #[skyzen::durable_object]
    struct CounterObject;

    impl DurableObject for CounterObject {
        fn fetch(&mut self) -> crate::routing::Router {
            Route::new((
                "/increment".post(increment),
                "/slow_increment".post(slow_increment),
                "/value".at(value),
                "/alarm_count".at(alarm_count),
                "/schedule_alarm".post(schedule_alarm),
            ))
            .on_alarm(run_alarm)
            .build()
        }
    }

    async fn increment(db: DurableDb) -> Result<String> {
        db.query("CREATE TABLE IF NOT EXISTS counter (value INTEGER NOT NULL)")
            .execute()
            .await?;

        let current = db
            .query("SELECT value FROM counter LIMIT 1")
            .fetch_scalar_optional::<i64>()
            .await?
            .unwrap_or(0);
        let next = current + 1;

        if current == 0 {
            db.query("INSERT INTO counter (value) VALUES (?)")
                .bind(next)
                .execute()
                .await?;
        } else {
            db.query("UPDATE counter SET value = ?")
                .bind(next)
                .execute()
                .await?;
        }

        Ok(next.to_string())
    }

    /// Read-modify-write with an await gap in the middle: if two same-id requests ran
    /// concurrently, both would read the same value and one update would be lost.
    async fn slow_increment(db: DurableDb) -> Result<String> {
        db.query("CREATE TABLE IF NOT EXISTS counter (value INTEGER NOT NULL)")
            .execute()
            .await?;

        let current = db
            .query("SELECT value FROM counter LIMIT 1")
            .fetch_scalar_optional::<i64>()
            .await?
            .unwrap_or(0);

        async_io::Timer::after(std::time::Duration::from_millis(50)).await;

        let next = current + 1;
        if current == 0 {
            db.query("INSERT INTO counter (value) VALUES (?)")
                .bind(next)
                .execute()
                .await?;
        } else {
            db.query("UPDATE counter SET value = ?")
                .bind(next)
                .execute()
                .await?;
        }

        Ok(next.to_string())
    }

    async fn schedule_alarm(alarm: skyzen_services::durable::Alarm) -> Result<&'static str> {
        alarm.set_alarm(super::current_unix_ms() + 50).await?;
        Ok("scheduled")
    }

    async fn value(db: DurableDb) -> Result<String> {
        db.query("CREATE TABLE IF NOT EXISTS counter (value INTEGER NOT NULL)")
            .execute()
            .await?;

        let current = db
            .query("SELECT value FROM counter LIMIT 1")
            .fetch_scalar_optional::<i64>()
            .await?
            .unwrap_or(0);
        Ok(current.to_string())
    }

    async fn run_alarm(db: DurableDb) -> Result<&'static str> {
        db.query("CREATE TABLE IF NOT EXISTS alarm_runs (value INTEGER NOT NULL)")
            .execute()
            .await?;

        let current = db
            .query("SELECT value FROM alarm_runs LIMIT 1")
            .fetch_scalar_optional::<i64>()
            .await?
            .unwrap_or(0);
        let next = current + 1;

        if current == 0 {
            db.query("INSERT INTO alarm_runs (value) VALUES (?)")
                .bind(next)
                .execute()
                .await?;
        } else {
            db.query("UPDATE alarm_runs SET value = ?")
                .bind(next)
                .execute()
                .await?;
        }

        Ok("ok")
    }

    async fn alarm_count(db: DurableDb) -> Result<String> {
        db.query("CREATE TABLE IF NOT EXISTS alarm_runs (value INTEGER NOT NULL)")
            .execute()
            .await?;

        let current = db
            .query("SELECT value FROM alarm_runs LIMIT 1")
            .fetch_scalar_optional::<i64>()
            .await?
            .unwrap_or(0);
        Ok(current.to_string())
    }

    /// Framework-managed state: the hit count survives between events because the runtime
    /// serializes the whole object after each one.
    #[derive(Default, Serialize, Deserialize)]
    struct BlobCounter {
        hits: u64,
    }

    impl DurableObject for BlobCounter {
        fn fetch(&mut self) -> crate::routing::Router {
            self.hits += 1;
            let hits = self.hits;
            Route::new(("/hits".at(move || async move { hits.to_string() }),)).build()
        }
    }

    /// The same shape with `PERSIST = false`: nothing is stored, so every event starts from
    /// `Default` and the count never climbs past one.
    #[derive(Default, Serialize, Deserialize)]
    struct ScratchCounter {
        hits: u64,
    }

    impl DurableObject for ScratchCounter {
        const PERSIST: bool = false;

        fn fetch(&mut self) -> crate::routing::Router {
            self.hits += 1;
            let hits = self.hits;
            Route::new(("/hits".at(move || async move { hits.to_string() }),)).build()
        }
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
    async fn persist_false_skips_the_framework_state_round_trip() {
        let blob = NativeDurableNamespace::<BlobCounter>::new();
        let stub = blob.get_by_name("blob").expect("blob object");
        for expected in ["1", "2", "3"] {
            let response = stub
                .fetch(request(Method::GET, "/hits"))
                .await
                .expect("blob hit");
            assert_eq!(response_text(response).await, expected);
        }

        let scratch = NativeDurableNamespace::<ScratchCounter>::new();
        let stub = scratch.get_by_name("scratch").expect("scratch object");
        for _ in 0..3u32 {
            let response = stub
                .fetch(request(Method::GET, "/hits"))
                .await
                .expect("scratch hit");
            // Nothing was saved, so the object is rebuilt from `Default` on every event.
            assert_eq!(response_text(response).await, "1");
        }
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
    async fn native_durable_namespace_serializes_same_id_dispatch() {
        let namespace = NativeDurableNamespace::<CounterObject>::new();
        let stub = namespace.get_by_name("serial").expect("serial object");

        let (first, second) = tokio::join!(
            stub.fetch(request(Method::POST, "/slow_increment")),
            stub.fetch(request(Method::POST, "/slow_increment")),
        );
        first.expect("first slow increment");
        second.expect("second slow increment");

        // Without per-object serialization both requests read 0 and the final value is 1.
        let value = stub
            .fetch(request(Method::GET, "/value"))
            .await
            .expect("value");
        assert_eq!(response_text(value).await, "2");
    }

    #[tokio::test]
    async fn native_alarm_scheduler_fires_scheduled_alarm() {
        let namespace = NativeDurableNamespace::<CounterObject>::new();
        let stub = namespace.get_by_name("timer-alarm").expect("alarm object");

        let response = stub
            .fetch(request(Method::POST, "/schedule_alarm"))
            .await
            .expect("schedule alarm");
        assert_eq!(response_text(response).await, "scheduled");

        // The background timer should invoke the alarm handler shortly after the deadline.
        for _ in 0..100u32 {
            async_io::Timer::after(std::time::Duration::from_millis(50)).await;
            let response = stub
                .fetch(request(Method::GET, "/alarm_count"))
                .await
                .expect("alarm count");
            if response_text(response).await == "1" {
                return;
            }
        }
        panic!("scheduled alarm never fired");
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
