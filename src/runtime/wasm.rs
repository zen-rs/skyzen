use std::{
    cell::RefCell,
    error::Error as StdError,
    fmt,
    future::{ready, Future},
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

use futures_core::Stream;
use http_kit::http_error;
use skyzen_core::Extractor;

use crate::{Body, BodyError, Endpoint, StatusCode};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Alias matching the `WinterCG` request object.
pub type Request = web_sys::Request;
/// Alias for the `WinterCG` response object.
pub type Response = web_sys::Response;
/// Alias for arbitrary environment bindings.
pub type Env = JsValue;
/// Alias for the execution context value.
pub type ExecutionContext = JsValue;

thread_local! {
    static CURRENT_ENV: RefCell<Option<JsValue>> = const { RefCell::new(None) };
    /// Type-erased cache of the endpoint built by the app factory, so the router and service
    /// bindings are constructed once per isolate instead of on every request.
    static CACHED_ENDPOINT: RefCell<Option<Box<dyn std::any::Any>>> = const { RefCell::new(None) };
}

/// Get the current `WinterCG` env during endpoint construction.
/// Can be called multiple times during the factory call in the fetch handler.
#[must_use]
pub fn current_env() -> Option<JsValue> {
    CURRENT_ENV.with_borrow(std::clone::Clone::clone)
}

fn set_current_env(env: JsValue) {
    CURRENT_ENV.with_borrow_mut(|slot| *slot = Some(env));
}

fn clear_current_env() {
    CURRENT_ENV.with_borrow_mut(|slot| *slot = None);
}

struct CurrentEnvGuard;

impl Drop for CurrentEnvGuard {
    fn drop(&mut self) {
        clear_current_env();
    }
}

/// Wrapper for `WinterCG` env, usable in request extensions.
/// SAFETY: WASM is single-threaded, so Send+Sync is safe.
#[derive(Clone, Debug)]
pub struct WasmEnv(JsValue);

unsafe impl Send for WasmEnv {}
unsafe impl Sync for WasmEnv {}

impl WasmEnv {
    /// Wrap a raw `WinterCG` environment value.
    #[must_use]
    pub const fn new(env: JsValue) -> Self {
        Self(env)
    }

    /// Get the inner `JsValue`.
    #[must_use]
    pub fn into_inner(self) -> JsValue {
        self.0
    }

    /// Get a reference to the inner `JsValue`.
    #[must_use]
    pub const fn as_js(&self) -> &JsValue {
        &self.0
    }
}

http_error!(
    /// The WinterCG environment was not found in request extensions.
    pub WasmEnvNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Wasm environment not configured. Ensure the runtime injected WasmEnv into request extensions."
);

impl Extractor for WasmEnv {
    type Error = WasmEnvNotConfigured;

    // Reading the environment back out of the extensions is a synchronous clone, so the future is
    // ready on creation rather than an `async` block with nothing to await.
    fn extract(
        request: &mut crate::Request,
    ) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or(WasmEnvNotConfigured::new()),
        )
    }
}

/// Temporarily make the WinterCG environment available via [`current_env`].
#[doc(hidden)]
pub fn with_current_env<T>(env: JsValue, f: impl FnOnce() -> T) -> T {
    set_current_env(env);
    let _guard = CurrentEnvGuard;
    f()
}

/// Bridge the annotated endpoint into the `WinterCG` `fetch` contract.
///
/// The factory receives the `WinterCG` environment explicitly (instead of via an ambient
/// thread-local), so concurrent invocations on the same isolate cannot race each other's
/// environment while the factory awaits. The built endpoint is cached per isolate thread and
/// cloned for each request, so the router and service bindings are constructed only once.
///
/// # Errors
///
/// Returns a `JsValue` error when the incoming request or outgoing response
/// cannot be converted across the JS boundary.
pub async fn launch<Fut, E>(
    factory: impl FnOnce(Env) -> Fut,
    request: Request,
    env: Env,
    ctx: ExecutionContext,
) -> Result<Response, JsValue>
where
    Fut: Future<Output = E>,
    E: Endpoint + Clone + 'static,
{
    let endpoint = if let Some(endpoint) = cached_endpoint::<E>() {
        endpoint
    } else {
        let endpoint = factory(env.clone()).await;
        store_cached_endpoint(endpoint.clone());
        endpoint
    };

    serve(endpoint, request, env, ctx).await
}

fn cached_endpoint<E: Clone + 'static>() -> Option<E> {
    CACHED_ENDPOINT.with_borrow(|slot| {
        slot.as_ref()
            .and_then(|endpoint| endpoint.downcast_ref::<E>())
            .cloned()
    })
}

fn store_cached_endpoint<E: 'static>(endpoint: E) {
    CACHED_ENDPOINT.with_borrow_mut(|slot| *slot = Some(Box::new(endpoint)));
}

async fn serve<E>(
    mut endpoint: E,
    request: Request,
    env: Env,
    ctx: ExecutionContext,
) -> Result<Response, JsValue>
where
    E: Endpoint + Clone + 'static,
{
    let mut sky_request = convert_request(&request)?;
    // Make WinterCG env available via request extensions
    sky_request.extensions_mut().insert(WasmEnv::new(env));
    // The execution context is what keeps post-response work alive: a future not awaited before
    // the response is returned is cancelled with the isolate unless it goes through `waitUntil`.
    sky_request
        .extensions_mut()
        .insert(super::WorkerContext::new(ctx));

    // Capture the request identity before `respond` takes the request mutably, so the error log
    // can name the call that failed the way the native backends do.
    let method = sky_request.method().clone();
    let path = sky_request.uri().path().to_owned();

    let response = match endpoint.respond(&mut sky_request).await {
        Ok(response) => response,
        Err(error) => {
            // Log and render through the shared helpers so every backend emits the same fields
            // and applies the same 4xx/5xx redaction policy.
            skyzen_core::log_endpoint_error(&error, &method, path.as_str());
            skyzen_core::error_response(&error)
        }
    };

    convert_response(response)
}

fn convert_request(request: &Request) -> Result<crate::Request, JsValue> {
    let method = request
        .method()
        .parse::<http::Method>()
        .map_err(|error| JsValue::from_str(&format!("Invalid method `{error}`")))?;
    let mut builder = http::Request::builder().method(method).uri(request.url());

    let headers = request.headers();
    let iter = js_sys::try_iter(&headers)?
        .ok_or_else(|| JsValue::from_str("Headers iterator unavailable"))?;

    for entry in iter {
        let entry = entry?;
        let pair = js_sys::Array::from(&entry);
        let key = pair
            .get(0)
            .as_string()
            .ok_or_else(|| JsValue::from_str("Invalid header name"))?;
        let value = pair
            .get(1)
            .as_string()
            .ok_or_else(|| JsValue::from_str("Invalid header value"))?;
        builder = builder.header(key, value);
    }

    let http_request = builder
        .body(request_body(request))
        .map_err(|error| JsValue::from_str(&format!("Failed to build request: {error}")))?;
    let mut sky_request = crate::Request::from(http_request);

    // `cf` is Cloudflare's, not WinterCG's, so it is simply absent on other hosts — which is not
    // an error, just a request with no edge metadata to extract.
    if let Some(slot) = super::CfPropertiesSlot::read(request)? {
        sky_request.extensions_mut().insert(slot);
    }

    Ok(sky_request)
}

fn convert_response(response: crate::Response) -> Result<Response, JsValue> {
    // Only the websocket-extension handling below needs mutable access.
    #[cfg(feature = "ws")]
    let mut response = response;

    // Handle WebSocket upgrade responses (status 101)
    #[cfg(feature = "ws")]
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(ws) = response
            .extensions_mut()
            .remove::<crate::durable::DurableClientWebSocket>()
        {
            let init = web_sys::ResponseInit::new();
            init.set_status(StatusCode::SWITCHING_PROTOCOLS.as_u16());
            js_sys::Reflect::set(init.as_ref(), &"webSocket".into(), ws.0.as_ref())?;
            return web_sys::Response::new_with_opt_buffer_source_and_init(None, &init);
        }

        if let Some(ws) = response
            .extensions_mut()
            .remove::<crate::websocket::SendSyncWebSocket>()
        {
            return crate::websocket::create_websocket_response(&ws.into_inner());
        }
    }

    // A 101 without an attached WebSocket cannot be represented by the standard Response
    // constructor (it throws an opaque RangeError); fail with a clear error instead.
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        tracing::error!("101 response without an attached WebSocket; returning 500");
        let init = web_sys::ResponseInit::new();
        init.set_status(StatusCode::INTERNAL_SERVER_ERROR.as_u16());
        return Response::new_with_opt_str_and_init(
            Some("101 Switching Protocols response was missing a WebSocket"),
            &init,
        );
    }

    let status = response.status().as_u16();
    let init = web_sys::ResponseInit::new();
    init.set_status(status);
    init.set_status_text(response.status().canonical_reason().unwrap_or("OK"));

    let headers = web_sys::Headers::new()?;
    for (key, value) in response.headers() {
        // Header values are not guaranteed to be ASCII; use a lossy UTF-8 view rather than
        // silently dropping non-ASCII values.
        headers.append(key.as_str(), &String::from_utf8_lossy(value.as_bytes()))?;
    }
    init.set_headers(&headers);

    let body = response_body_stream(response.into_body());
    Response::new_with_opt_readable_stream_and_init(Some(&body), &init)
}

fn request_body(request: &Request) -> Body {
    let Some(raw_stream) = request.body() else {
        return Body::empty();
    };

    let stream = wasm_streams::ReadableStream::from_raw(raw_stream).into_stream();
    Body::from_stream(JsReadableBody { inner: stream })
}

fn response_body_stream(body: Body) -> web_sys::ReadableStream {
    wasm_streams::ReadableStream::from_stream(BodyReadableStream { body }).into_raw()
}

#[derive(Debug)]
struct JsReadableBody {
    inner: wasm_streams::readable::IntoStream<'static>,
}

// SAFETY: WinterCG WASM request streams are confined to the single-threaded JS event loop.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for JsReadableBody {}
// SAFETY: `JsReadableBody` is only polled by the single-threaded WASM runtime.
unsafe impl Sync for JsReadableBody {}

impl Stream for JsReadableBody {
    type Item = Result<Vec<u8>, BodyError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx).map(|item| {
            item.map(|result| match result {
                Ok(value) => js_value_to_body_bytes(&value),
                Err(error) => Err(body_stream_error(js_value_to_error_message(&error))),
            })
        })
    }
}

#[derive(Debug)]
struct BodyReadableStream {
    body: Body,
}

impl Stream for BodyReadableStream {
    type Item = Result<JsValue, JsValue>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.body).poll_next(cx).map(|item| {
            item.map(|result| {
                result
                    .map(|bytes| bytes_to_js_value(&bytes))
                    .map_err(|error| body_error_to_js(&error))
            })
        })
    }
}

fn js_value_to_body_bytes(value: &JsValue) -> Result<Vec<u8>, BodyError> {
    if value.is_instance_of::<js_sys::Uint8Array>() || value.is_instance_of::<js_sys::ArrayBuffer>()
    {
        Ok(js_sys::Uint8Array::new(value).to_vec())
    } else {
        Err(body_stream_error(
            "request body stream yielded a non-byte chunk",
        ))
    }
}

fn bytes_to_js_value(bytes: &bytes::Bytes) -> JsValue {
    js_sys::Uint8Array::from(bytes.as_ref()).into()
}

fn body_error_to_js(error: &BodyError) -> JsValue {
    JsValue::from_str(&error.to_string())
}

fn js_value_to_error_message(value: &JsValue) -> String {
    value
        .as_string()
        .unwrap_or_else(|| format!("request body stream read failed: {value:?}"))
}

fn body_stream_error(message: impl Into<String>) -> BodyError {
    BodyError::Other(Box::new(WasmBodyStreamError(message.into())))
}

#[derive(Debug)]
struct WasmBodyStreamError(String);

impl fmt::Display for WasmBodyStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for WasmBodyStreamError {}
