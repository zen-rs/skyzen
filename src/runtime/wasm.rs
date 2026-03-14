use std::cell::RefCell;
use std::future::Future;

use http_kit::http_error;
use skyzen_core::Extractor;

use crate::{Body, Endpoint, HttpError, StatusCode};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Alias matching the WinterCG request object.
pub type Request = web_sys::Request;
/// Alias for the WinterCG response object.
pub type Response = web_sys::Response;
/// Alias for arbitrary environment bindings.
pub type Env = JsValue;
/// Alias for the execution context value.
pub type ExecutionContext = JsValue;

thread_local! {
    static CURRENT_ENV: RefCell<Option<JsValue>> = const { RefCell::new(None) };
}

/// Get the current WinterCG env during endpoint construction.
/// Can be called multiple times during the factory call in the fetch handler.
pub fn current_env() -> Option<JsValue> {
    CURRENT_ENV.with_borrow(|env| env.clone())
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

/// Wrapper for WinterCG env, usable in request extensions.
/// SAFETY: WASM is single-threaded, so Send+Sync is safe.
#[derive(Clone, Debug)]
pub struct WasmEnv(JsValue);

unsafe impl Send for WasmEnv {}
unsafe impl Sync for WasmEnv {}

impl WasmEnv {
    /// Wrap a raw WinterCG environment value.
    #[must_use]
    pub fn new(env: JsValue) -> Self {
        Self(env)
    }

    /// Get the inner JsValue.
    pub fn into_inner(self) -> JsValue {
        self.0
    }

    /// Get a reference to the inner JsValue.
    pub fn as_js(&self) -> &JsValue {
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

    async fn extract(request: &mut crate::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(WasmEnvNotConfigured::new())
    }
}

/// Temporarily make the WinterCG environment available via [`current_env`].
#[doc(hidden)]
pub fn with_current_env<T>(env: JsValue, f: impl FnOnce() -> T) -> T {
    set_current_env(env);
    let _guard = CurrentEnvGuard;
    f()
}

/// Bridge the annotated endpoint into the WinterCG `fetch` contract.
pub async fn launch<Fut, E>(
    factory: impl FnOnce() -> Fut,
    request: Request,
    env: Env,
    ctx: ExecutionContext,
) -> Result<Response, JsValue>
where
    Fut: Future<Output = E>,
    E: Endpoint + Clone + 'static,
{
    // Make env available during factory construction
    set_current_env(env.clone());
    let endpoint = factory().await;
    clear_current_env();

    serve(endpoint, request, env, ctx).await
}

async fn serve<E>(
    mut endpoint: E,
    request: Request,
    env: Env,
    _ctx: ExecutionContext,
) -> Result<Response, JsValue>
where
    E: Endpoint + Clone + 'static,
{
    let mut sky_request = convert_request(request).await?;
    // Make WinterCG env available via request extensions
    sky_request.extensions_mut().insert(WasmEnv::new(env));

    let response = match endpoint.respond(&mut sky_request).await {
        Ok(response) => response,
        Err(error) => error_to_response(error),
    };

    convert_response(response).await
}

/// Convert an HttpError to an HTTP response.
///
/// For server errors (5xx), the error message is hidden to avoid leaking internals.
/// For client errors (4xx) and others, the error message is included in the response.
fn error_to_response(error: impl HttpError) -> crate::Response {
    let status = error.status();
    let error_message = error.to_string();

    // For 5xx server errors, hide internal details
    // For 4xx client errors and others, show the error message
    let body_message = if status.is_server_error() {
        tracing::error!(
            status = status.as_u16(),
            error = %error_message,
            "Internal server error"
        );
        "Internal server error".to_string()
    } else {
        tracing::warn!(
            status = status.as_u16(),
            error = %error_message,
            "Client error"
        );
        error_message
    };

    // Create JSON error response using serde_json for proper escaping
    let body = serde_json::json!({ "error": body_message }).to_string();

    let mut response = crate::Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::header::HeaderValue::from_static("application/json"),
    );
    response
}

async fn convert_request(request: Request) -> Result<crate::Request, JsValue> {
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

    let bytes = read_body_bytes(&request).await?;
    let http_request = builder
        .body(Body::from(bytes))
        .map_err(|error| JsValue::from_str(&format!("Failed to build request: {error}")))?;
    Ok(crate::Request::from(http_request))
}

async fn convert_response(mut response: crate::Response) -> Result<Response, JsValue> {
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

    let status = response.status().as_u16();
    let init = web_sys::ResponseInit::new();
    init.set_status(status);
    init.set_status_text(response.status().canonical_reason().unwrap_or("OK"));

    let headers = web_sys::Headers::new()?;
    for (key, value) in response.headers().iter() {
        headers.append(key.as_str(), value.to_str().unwrap_or_default())?;
    }
    init.set_headers(&headers);

    let bytes = response
        .into_body()
        .into_bytes()
        .await
        .map_err(|error| JsValue::from_str(&error.to_string()))?;

    // Use Uint8Array to safely pass bytes to JavaScript
    // This avoids memory safety issues with direct slice passing
    let uint8_array = js_sys::Uint8Array::from(bytes.as_ref());
    Response::new_with_opt_buffer_source_and_init(Some(&uint8_array), &init)
}

async fn read_body_bytes(request: &Request) -> Result<Vec<u8>, JsValue> {
    let promise = request
        .array_buffer()
        .map_err(|error| JsValue::from(error))?;
    let buffer = JsFuture::from(promise).await?;
    let array = js_sys::Uint8Array::new(&buffer);
    Ok(array.to_vec())
}
