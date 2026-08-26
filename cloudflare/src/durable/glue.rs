//! Runtime glue for driving Skyzen Durable Objects on Cloudflare Workers.

use std::marker::PhantomData;

use skyzen::durable::{DurableObject, DurableObjectError, WebSocketConnection, WebSocketEvent};
use skyzen::{Body, Endpoint, Method, Request, Response, StatusCode, Uri};
use skyzen_services::durable::DurableKv;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker_sys::ext::ResponseInitExt;

use super::{
    kv::CfDurableKv,
    state::CfDurableState,
    websocket::{clone_state, CfWebSocketConnection},
};

use super::STATE_KEY as SKYZEN_STATE_KEY;

const ALARM_REQUEST_PATH: &str = "/__skyzen_alarm";

/// A Durable Object loaded from storage together with the serialized bytes it
/// was restored from, so an unchanged object can skip the storage write.
struct LoadedObject<T> {
    object: T,
    snapshot: Option<Vec<u8>>,
}

/// Cloudflare runtime adapter for a Skyzen Durable Object type.
#[derive(Debug)]
pub struct DurableObjectRuntime<T>(PhantomData<T>);

impl<T> DurableObjectRuntime<T>
where
    T: DurableObject,
{
    /// Handle a Durable Object `fetch` event.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O or request/response conversion fails.
    pub async fn fetch(
        state: worker_sys::DurableObjectState,
        env: JsValue,
        request: web_sys::Request,
    ) -> Result<web_sys::Response, JsValue> {
        let mut loaded = load_state::<T>(&state).await?;
        let durable_state = CfDurableState::new(clone_state(&state), env.clone());

        let mut request = convert_request(request).await?;
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
                skyzen::runtime::wasm::with_current_env(env, || loaded.object.fetch());
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
            save_state(&state, &loaded).await?;
        }
        convert_response(response).await
    }

    /// Handle a Durable Object `alarm` event.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O, alarm dispatch, or persistence fails.
    pub async fn alarm(state: worker_sys::DurableObjectState, env: JsValue) -> Result<(), JsValue> {
        let mut loaded = load_state::<T>(&state).await?;
        let durable_state = CfDurableState::new(clone_state(&state), env.clone());

        {
            // `fetch()` returns a `Router`, which exposes the alarm handler registered via
            // `Route::on_alarm` directly — no runtime downcast required.
            let router = skyzen::runtime::wasm::with_current_env(env, || loaded.object.fetch());

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
        }

        save_state(&state, &loaded).await
    }

    /// Handle a Durable Object `webSocketMessage` event.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O, message decoding, handler execution,
    /// or persistence fails.
    pub async fn websocket_message(
        state: worker_sys::DurableObjectState,
        env: JsValue,
        websocket: web_sys::WebSocket,
        event: web_sys::MessageEvent,
    ) -> Result<(), JsValue> {
        let mut loaded = load_state::<T>(&state).await?;
        let durable_state = CfDurableState::new(clone_state(&state), env);
        let context = durable_state.context().map_err(to_js)?;
        let connection = WebSocketConnection::new(Box::new(CfWebSocketConnection::new(
            websocket,
            clone_state(&state),
        )));
        let data = event.data();
        let message = decode_websocket_message(&data).map_err(to_js)?;

        loaded
            .object
            .websocket(&connection, WebSocketEvent::Message(message), &context)
            .await
            .map_err(to_js)?;

        save_state(&state, &loaded).await
    }

    /// Handle a Durable Object `webSocketClose` event.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O, handler execution, or persistence fails.
    pub async fn websocket_close(
        state: worker_sys::DurableObjectState,
        env: JsValue,
        websocket: web_sys::WebSocket,
        code: u16,
        reason: String,
        was_clean: bool,
    ) -> Result<(), JsValue> {
        let mut loaded = load_state::<T>(&state).await?;
        let durable_state = CfDurableState::new(clone_state(&state), env);
        let context = durable_state.context().map_err(to_js)?;
        let connection = WebSocketConnection::new(Box::new(CfWebSocketConnection::new(
            websocket,
            clone_state(&state),
        )));

        loaded
            .object
            .websocket(
                &connection,
                WebSocketEvent::Close {
                    code,
                    reason,
                    was_clean,
                },
                &context,
            )
            .await
            .map_err(to_js)?;

        save_state(&state, &loaded).await
    }

    /// Handle a Durable Object `webSocketError` event.
    ///
    /// # Errors
    ///
    /// Returns `JsValue` when state I/O, handler execution, or persistence fails.
    pub async fn websocket_error(
        state: worker_sys::DurableObjectState,
        env: JsValue,
        websocket: web_sys::WebSocket,
        error: JsValue,
    ) -> Result<(), JsValue> {
        let mut loaded = load_state::<T>(&state).await?;
        let durable_state = CfDurableState::new(clone_state(&state), env);
        let context = durable_state.context().map_err(to_js)?;
        let connection = WebSocketConnection::new(Box::new(CfWebSocketConnection::new(
            websocket,
            clone_state(&state),
        )));

        loaded
            .object
            .websocket(
                &connection,
                WebSocketEvent::Error(format!("{error:?}")),
                &context,
            )
            .await
            .map_err(to_js)?;

        save_state(&state, &loaded).await
    }
}

/// Invoke a Skyzen Durable Object alarm handler from an external runtime wrapper.
///
/// # Errors
///
/// Returns `JsValue` when state I/O, alarm dispatch, or persistence fails.
pub async fn invoke_alarm<T>(
    state: worker_sys::DurableObjectState,
    env: JsValue,
) -> Result<(), JsValue>
where
    T: DurableObject,
{
    DurableObjectRuntime::<T>::alarm(state, env).await
}

/// Invoke a Skyzen Durable Object websocket message handler from an external runtime wrapper.
///
/// # Errors
///
/// Returns `JsValue` when state I/O, handler execution, or persistence fails.
pub async fn invoke_websocket_message<T>(
    state: worker_sys::DurableObjectState,
    env: JsValue,
    websocket: web_sys::WebSocket,
    message: skyzen::http_kit::ws::WebSocketMessage,
) -> Result<(), JsValue>
where
    T: DurableObject,
{
    let mut loaded = load_state::<T>(&state).await?;
    let durable_state = CfDurableState::new(clone_state(&state), env);
    let context = durable_state.context().map_err(to_js)?;
    let connection = WebSocketConnection::new(Box::new(CfWebSocketConnection::new(
        websocket,
        clone_state(&state),
    )));

    loaded
        .object
        .websocket(&connection, WebSocketEvent::Message(message), &context)
        .await
        .map_err(to_js)?;

    save_state(&state, &loaded).await
}

/// Invoke a Skyzen Durable Object websocket close handler from an external runtime wrapper.
///
/// # Errors
///
/// Returns `JsValue` when state I/O, handler execution, or persistence fails.
pub async fn invoke_websocket_close<T>(
    state: worker_sys::DurableObjectState,
    env: JsValue,
    websocket: web_sys::WebSocket,
    code: u16,
    reason: String,
    was_clean: bool,
) -> Result<(), JsValue>
where
    T: DurableObject,
{
    DurableObjectRuntime::<T>::websocket_close(state, env, websocket, code, reason, was_clean).await
}

/// Invoke a Skyzen Durable Object websocket error handler from an external runtime wrapper.
///
/// # Errors
///
/// Returns `JsValue` when state I/O, handler execution, or persistence fails.
pub async fn invoke_websocket_error<T>(
    state: worker_sys::DurableObjectState,
    env: JsValue,
    websocket: web_sys::WebSocket,
    error: String,
) -> Result<(), JsValue>
where
    T: DurableObject,
{
    let mut loaded = load_state::<T>(&state).await?;
    let durable_state = CfDurableState::new(clone_state(&state), env);
    let context = durable_state.context().map_err(to_js)?;
    let connection = WebSocketConnection::new(Box::new(CfWebSocketConnection::new(
        websocket,
        clone_state(&state),
    )));

    loaded
        .object
        .websocket(&connection, WebSocketEvent::Error(error), &context)
        .await
        .map_err(to_js)?;

    save_state(&state, &loaded).await
}

async fn load_state<T>(state: &worker_sys::DurableObjectState) -> Result<LoadedObject<T>, JsValue>
where
    T: DurableObject,
{
    // An object that keeps its state in storage itself has no blob to restore, so the read and the
    // parse are skipped rather than performed and discarded.
    if !T::PERSIST {
        return Ok(LoadedObject {
            object: T::default(),
            snapshot: None,
        });
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
    Ok(LoadedObject {
        object,
        snapshot: maybe_bytes,
    })
}

/// Persist the object's serialized state, skipping the storage write when the
/// bytes are identical to what [`load_state`] read (read-only events would
/// otherwise write on every invocation), and skipping it entirely for an object
/// that opted out with `PERSIST = false`.
async fn save_state<T>(
    state: &worker_sys::DurableObjectState,
    loaded: &LoadedObject<T>,
) -> Result<(), JsValue>
where
    T: DurableObject,
{
    if !T::PERSIST {
        return Ok(());
    }

    let bytes = serde_json::to_vec(&loaded.object).map_err(|error| {
        JsValue::from_str(&format!(
            "failed to serialize durable state '{SKYZEN_STATE_KEY}': {error}"
        ))
    })?;
    if loaded.snapshot.as_deref() == Some(bytes.as_slice()) {
        return Ok(());
    }
    let kv = DurableKv::new(CfDurableKv::from_state(state).map_err(|error| {
        JsValue::from_str(&format!(
            "failed to initialize durable kv for state save: {error}"
        ))
    })?);
    kv.put(SKYZEN_STATE_KEY, &bytes)
        .await
        .map_err(|error| JsValue::from_str(&format!("failed to persist durable state: {error}")))
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

async fn convert_request(request: web_sys::Request) -> Result<Request, JsValue> {
    let method = request
        .method()
        .parse::<Method>()
        .map_err(|error| JsValue::from_str(&format!("invalid request method: {error}")))?;
    let uri = request
        .url()
        .parse::<Uri>()
        .map_err(|error| JsValue::from_str(&format!("invalid request URI: {error}")))?;

    let bytes = read_body_bytes(&request).await?;
    let mut sky_request = Request::new(Body::from(bytes));
    *sky_request.method_mut() = method;
    *sky_request.uri_mut() = uri;

    let headers = request.headers();
    let iter = js_sys::try_iter(&headers)?
        .ok_or_else(|| JsValue::from_str("Headers iterator unavailable"))?;
    for entry in iter {
        let entry = entry?;
        let pair = js_sys::Array::from(&entry);
        let key = pair
            .get(0)
            .as_string()
            .ok_or_else(|| JsValue::from_str("invalid header name"))?;
        let value = pair
            .get(1)
            .as_string()
            .ok_or_else(|| JsValue::from_str("invalid header value"))?;

        let name = skyzen::header::HeaderName::from_bytes(key.as_bytes()).map_err(|error| {
            JsValue::from_str(&format!("failed to parse header name '{key}': {error}"))
        })?;
        let value = skyzen::header::HeaderValue::from_str(&value).map_err(|error| {
            JsValue::from_str(&format!(
                "failed to parse header value for '{key}': {error}"
            ))
        })?;
        sky_request.headers_mut().insert(name, value);
    }

    Ok(sky_request)
}

async fn convert_response(mut response: Response) -> Result<web_sys::Response, JsValue> {
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(socket) = response
            .extensions_mut()
            .remove::<skyzen::durable::DurableClientWebSocket>()
        {
            let mut init = web_sys::ResponseInit::new();
            init.set_status(StatusCode::SWITCHING_PROTOCOLS.as_u16());
            init.websocket(&socket.0)?;
            return web_sys::Response::new_with_opt_buffer_source_and_init(None, &init);
        }

        return Err(JsValue::from_str(
            "status 101 response missing DurableClientWebSocket extension",
        ));
    }

    let init = web_sys::ResponseInit::new();
    init.set_status(response.status().as_u16());
    init.set_status_text(response.status().canonical_reason().unwrap_or("OK"));

    let headers = web_sys::Headers::new()?;
    for (key, value) in response.headers() {
        // Header values are not guaranteed to be ASCII; a lossy UTF-8 view
        // preserves them instead of silently dropping non-ASCII values.
        headers.append(key.as_str(), &String::from_utf8_lossy(value.as_bytes()))?;
    }
    init.set_headers(&headers);

    let bytes = response
        .into_body()
        .into_bytes()
        .await
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    let body = js_sys::Uint8Array::from(bytes.as_ref());
    web_sys::Response::new_with_opt_buffer_source_and_init(Some(&body), &init)
}

async fn read_body_bytes(request: &web_sys::Request) -> Result<Vec<u8>, JsValue> {
    let promise = request.array_buffer()?;
    let buffer = JsFuture::from(promise).await?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

#[allow(clippy::needless_pass_by_value)]
fn to_js(error: DurableObjectError) -> JsValue {
    JsValue::from_str(&error.to_string())
}
