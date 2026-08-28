use std::{
    cell::RefCell,
    future::{ready, Future},
};

use http_kit::http_error;
use skyzen_core::Extractor;

use crate::{Endpoint, StatusCode};
use wasm_bindgen::prelude::*;

mod convert;

pub use convert::{from_js_request, from_js_response, into_js_request, into_js_response};

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
    let mut sky_request = from_js_request(&request)?;
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

    into_js_response(response)
}
