//! Runtime glue for driving Skyzen Durable Objects on Cloudflare Workers.
//!
//! One [`DurableObjectRuntime`] is the Rust side of one Durable Object instance: the class wrapper
//! `#[skyzen::durable_object]` exports constructs it once, when the platform constructs the JS
//! instance, and forwards every event on that instance to it. The user's object is built on the
//! first event — from the state blob when the type [persists](DurableObject::PERSIST), from
//! `Default` otherwise — and lives until the platform evicts the instance, so a field set by one
//! event is there for the next.

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use skyzen::durable::{DurableObject, DurableObjectError, WebSocketConnection, WebSocketEvent};
use skyzen::runtime::wasm::{from_js_request, into_js_response};
use skyzen::{Body, Endpoint, Method, Request, Uri};
use skyzen_services::durable::DurableKv;
use wasm_bindgen::{JsCast, JsValue};

use super::{
    kv::CfDurableKv,
    state::CfDurableState,
    websocket::{clone_state, CfWebSocketConnection},
};

use super::STATE_KEY as SKYZEN_STATE_KEY;

const ALARM_REQUEST_PATH: &str = "/__skyzen_alarm";

/// The Rust side of one Durable Object instance.
///
/// Cloning yields another handle on the same instance — the wrapper hands one to every event's
/// future — so the object and the snapshot it was last written from are shared, never copied.
pub struct DurableObjectRuntime<T> {
    state: worker_sys::DurableObjectState,
    env: JsValue,
    instance: Rc<Instance<T>>,
}

/// What lives for the instance: the object, and the bytes it was last loaded from or saved as.
struct Instance<T> {
    object: OnceCell<Rc<T>>,
    /// The serialization storage holds, so a read-only event does not write it back.
    snapshot: RefCell<Option<Vec<u8>>>,
}

impl<T> Clone for DurableObjectRuntime<T> {
    fn clone(&self) -> Self {
        Self {
            state: clone_state(&self.state),
            env: self.env.clone(),
            instance: Rc::clone(&self.instance),
        }
    }
}

impl<T> std::fmt::Debug for DurableObjectRuntime<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableObjectRuntime")
            .field("loaded", &self.instance.object.get().is_some())
            .finish_non_exhaustive()
    }
}

impl<T> DurableObjectRuntime<T>
where
    T: DurableObject,
{
    /// The runtime for the instance the platform just constructed.
    ///
    /// Nothing is read here: the constructor is synchronous, so the object is loaded by the
    /// first event.
    #[must_use]
    pub fn new(state: worker_sys::DurableObjectState, env: JsValue) -> Self {
        Self {
            state,
            env,
            instance: Rc::new(Instance {
                object: OnceCell::new(),
                snapshot: RefCell::new(None),
            }),
        }
    }

    /// Handle a Durable Object `fetch` event.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O or request/response conversion fails.
    pub async fn fetch(&self, request: web_sys::Request) -> Result<web_sys::Response, JsValue> {
        let object = self.object().await?;
        let durable_state = CfDurableState::new(clone_state(&self.state), self.env.clone());

        let mut request = from_js_request(&request).map_err(annotate_conversion)?;
        durable_state
            .inject_request_extensions(&mut request)
            .map_err(to_js)?;

        // Capture the request identity before `respond` takes the request mutably, so the error
        // log can name the call that failed the way the HTTP backends do.
        let method = request.method().clone();
        let path = request.uri().path().to_owned();

        // State is persisted only when the handler succeeds, matching the
        // websocket and alarm paths.
        let (response, succeeded) = {
            let mut endpoint =
                skyzen::runtime::wasm::with_current_env(self.env.clone(), || object.fetch());
            match endpoint.respond(&mut request).await {
                Ok(response) => (response, true),
                Err(error) => {
                    // Log and render through the shared helpers so every backend emits the same
                    // fields and applies the same 4xx/5xx redaction policy.
                    skyzen::log_endpoint_error(&error, &method, path.as_str());
                    (skyzen::error_response(&error), false)
                }
            }
        };

        if succeeded {
            self.save(&object).await?;
        }
        into_js_response(response).map_err(annotate_conversion)
    }

    /// Handle a Durable Object `alarm` event.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O, alarm dispatch, or persistence fails.
    pub async fn alarm(&self) -> Result<(), JsValue> {
        let object = self.object().await?;
        let durable_state = CfDurableState::new(clone_state(&self.state), self.env.clone());

        // `fetch()` returns a `Router`, which exposes the alarm handler registered via
        // `Route::on_alarm` directly — no runtime downcast required.
        let router = skyzen::runtime::wasm::with_current_env(self.env.clone(), || object.fetch());

        let mut alarm_endpoint = router.alarm_endpoint().ok_or_else(|| {
            JsValue::from_str("No alarm handler registered. Use Route::on_alarm(handler).")
        })?;

        let mut request = alarm_request()?;
        durable_state
            .inject_request_extensions(&mut request)
            .map_err(to_js)?;

        alarm_endpoint
            .respond(&mut request)
            .await
            .map_err(|error| JsValue::from_str(&format!("alarm handler failed: {error}")))?;

        self.save(&object).await
    }

    /// Handle a Durable Object `webSocketMessage` event.
    ///
    /// `message` is the payload as the platform delivers it: a string, or an `ArrayBuffer`.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O, message decoding, handler execution,
    /// or persistence fails.
    pub async fn websocket_message(
        &self,
        websocket: web_sys::WebSocket,
        message: JsValue,
    ) -> Result<(), JsValue> {
        let message = decode_websocket_message(&message).map_err(to_js)?;
        self.websocket_event(websocket, WebSocketEvent::Message(message))
            .await
    }

    /// Handle a Durable Object `webSocketClose` event.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O, handler execution, or persistence fails.
    pub async fn websocket_close(
        &self,
        websocket: web_sys::WebSocket,
        code: u16,
        reason: String,
        was_clean: bool,
    ) -> Result<(), JsValue> {
        self.websocket_event(
            websocket,
            WebSocketEvent::Close {
                code,
                reason,
                was_clean,
            },
        )
        .await
    }

    /// Handle a Durable Object `webSocketError` event.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O, handler execution, or persistence fails.
    pub async fn websocket_error(
        &self,
        websocket: web_sys::WebSocket,
        error: JsValue,
    ) -> Result<(), JsValue> {
        self.websocket_event(websocket, WebSocketEvent::Error(format!("{error:?}")))
            .await
    }

    async fn websocket_event(
        &self,
        websocket: web_sys::WebSocket,
        event: WebSocketEvent,
    ) -> Result<(), JsValue> {
        let object = self.object().await?;
        let durable_state = CfDurableState::new(clone_state(&self.state), self.env.clone());
        let context = durable_state.context().map_err(to_js)?;
        let connection = WebSocketConnection::new(Box::new(CfWebSocketConnection::new(
            websocket,
            clone_state(&self.state),
        )));

        object
            .websocket(&connection, event, &context)
            .await
            .map_err(to_js)?;

        self.save(&object).await
    }

    /// The instance's object, loaded on the first call.
    ///
    /// The load is a storage read, and the platform delivers no other event to the instance
    /// while one is in progress, so the first event's load is the only one.
    async fn object(&self) -> Result<Rc<T>, JsValue> {
        if let Some(object) = self.instance.object.get() {
            return Ok(Rc::clone(object));
        }
        let (object, snapshot) = load_state::<T>(&self.state).await?;
        *self.instance.snapshot.borrow_mut() = snapshot;
        Ok(Rc::clone(
            self.instance.object.get_or_init(|| Rc::new(object)),
        ))
    }

    /// Persist the object's serialized state, skipping the storage write when the bytes are
    /// identical to the last load or save (read-only events would otherwise write on every
    /// invocation), and skipping it entirely for an object that opted out with `PERSIST = false`.
    async fn save(&self, object: &T) -> Result<(), JsValue> {
        if !T::PERSIST {
            return Ok(());
        }

        let bytes = serde_json::to_vec(object).map_err(|error| {
            JsValue::from_str(&format!(
                "failed to serialize durable state '{SKYZEN_STATE_KEY}': {error}"
            ))
        })?;
        if self.instance.snapshot.borrow().as_deref() == Some(bytes.as_slice()) {
            return Ok(());
        }
        let kv = DurableKv::new(CfDurableKv::from_state(&self.state).map_err(|error| {
            JsValue::from_str(&format!(
                "failed to initialize durable kv for state save: {error}"
            ))
        })?);
        kv.put(SKYZEN_STATE_KEY, &bytes).await.map_err(|error| {
            JsValue::from_str(&format!("failed to persist durable state: {error}"))
        })?;
        *self.instance.snapshot.borrow_mut() = Some(bytes);
        Ok(())
    }
}

/// Build the object from storage, together with the bytes it was restored from.
async fn load_state<T>(
    state: &worker_sys::DurableObjectState,
) -> Result<(T, Option<Vec<u8>>), JsValue>
where
    T: DurableObject,
{
    // An object that keeps its state in storage itself has no blob to restore, so the read and the
    // parse are skipped rather than performed and discarded.
    if !T::PERSIST {
        return Ok((T::default(), None));
    }

    let kv = DurableKv::new(CfDurableKv::from_state(state).map_err(|error| {
        JsValue::from_str(&format!(
            "failed to initialize durable kv for state load: {error}"
        ))
    })?);

    let maybe_bytes = kv.get(SKYZEN_STATE_KEY).await.map_err(|error| {
        JsValue::from_str(&format!("failed to load durable state bytes: {error}"))
    })?;
    let object = match &maybe_bytes {
        None => T::default(),
        Some(bytes) => serde_json::from_slice(bytes).map_err(|error| {
            JsValue::from_str(&format!(
                "failed to deserialize durable state '{SKYZEN_STATE_KEY}': {error}"
            ))
        })?,
    };
    Ok((object, maybe_bytes))
}

fn decode_websocket_message(
    data: &JsValue,
) -> Result<skyzen::http_kit::ws::WebSocketMessage, DurableObjectError> {
    if let Some(text) = data.as_string() {
        return Ok(skyzen::http_kit::ws::WebSocketMessage::Text(text.into()));
    }
    if data.is_instance_of::<js_sys::Uint8Array>() || data.is_instance_of::<js_sys::ArrayBuffer>() {
        let bytes = js_sys::Uint8Array::new(data).to_vec();
        return Ok(skyzen::http_kit::ws::WebSocketMessage::Binary(bytes.into()));
    }

    Err(DurableObjectError::WebSocket(format!(
        "unsupported websocket message payload: {data:?}"
    )))
}

fn alarm_request() -> Result<Request, JsValue> {
    let mut request = Request::new(Body::empty());
    *request.method_mut() = Method::GET;
    *request.uri_mut() = ALARM_REQUEST_PATH.parse::<Uri>().map_err(|error| {
        JsValue::from_str(&format!("failed to construct alarm request URI: {error}"))
    })?;
    Ok(request)
}

#[allow(clippy::needless_pass_by_value)]
fn to_js(error: DurableObjectError) -> JsValue {
    JsValue::from_str(&error.to_string())
}

/// Say which boundary a conversion failure came from.
///
/// The conversion itself is the framework's, and its messages name the offending method, header
/// or status; what it cannot know is that this particular crossing was a Durable Object's.
#[allow(clippy::needless_pass_by_value)]
fn annotate_conversion(error: JsValue) -> JsValue {
    JsValue::from_str(&format!(
        "durable object request/response conversion: {error:?}"
    ))
}
