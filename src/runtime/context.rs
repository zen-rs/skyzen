//! The request's execution context — work that outlives the response.

use core::{fmt, future::Future};

use http_kit::http_error;
use skyzen_core::Extractor;

use crate::StatusCode;

/// The runtime refused to take on post-response work.
#[derive(Debug)]
pub struct WorkerContextError(String);

impl WorkerContextError {
    /// Only the `wasm32` context can fail: natively the runtime that handed out the context can
    /// always take the task.
    #[cfg(target_arch = "wasm32")]
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for WorkerContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the runtime refused post-response work: {}", self.0)
    }
}

impl std::error::Error for WorkerContextError {}

impl http_kit::HttpError for WorkerContextError {
    fn status(&self) -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

http_error!(
    /// The runtime did not put an execution context into this request.
    pub WorkerContextNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Execution context not available. On Workers it is threaded in by `#[skyzen::main]`; \
     natively it is provided by the built-in runtime, not by an embedding host."
);

/// A handle on the runtime serving this request, for work that must outlive the response.
///
/// The motivating case is the same one Cloudflare documents for `ctx.waitUntil`: an analytics
/// write, a cache fill or a queue send that the client should not wait for. On Workers such a
/// future is *cancelled* the instant the response is returned unless the runtime is told to keep
/// it alive, so "just spawn it" silently does nothing there while working fine natively — the
/// exact asymmetry this type removes.
///
/// # Portability
///
/// [`wait_until`](Self::wait_until) exists on every target. On Workers it hands the future to the
/// platform's `waitUntil`; on the built-in native runtime it spawns the future *and* registers it
/// with graceful shutdown, so `Ctrl+C` waits for post-response work to finish rather than dropping
/// it on the floor.
///
/// [`pass_through_on_exception`](Self::pass_through_on_exception) is `wasm32`-only, deliberately.
/// Failing open to the origin is a Cloudflare routing concept with no native counterpart, and a
/// native no-op would be a silent divergence of exactly the kind this type exists to prevent — so
/// calling it in portable code is a compile error rather than a surprise in production.
///
/// # Availability
///
/// Natively the context comes from the built-in runtime that `#[skyzen::main]` starts. An
/// application embedding Skyzen through [`skyzen::hyper`](crate::hyper) runs its own server and
/// has no shutdown drain to register against, so extraction there fails with
/// [`WorkerContextNotConfigured`] rather than pretending to track the work.
#[derive(Clone, Debug)]
pub struct WorkerContext(Inner);

impl Extractor for WorkerContext {
    type Error = WorkerContextNotConfigured;

    async fn extract(request: &mut crate::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or_else(WorkerContextNotConfigured::new)
    }
}

// ── Native ──

/// A token that keeps the built-in runtime's graceful shutdown waiting on whatever holds it.
///
/// The runtime hands one to every connection task; [`WorkerContext::wait_until`] moves a further
/// clone into each spawned future, which is what makes shutdown wait for post-response work as
/// well as for in-flight requests. Nothing is ever sent through the channel — the runtime watches
/// for the last sender being dropped.
///
/// The module is private, so despite being `pub` this type is reachable only from inside the
/// crate — which is what seals [`WorkerContext::new`] on this target: nothing outside can name the
/// argument.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct ShutdownGuard(pub async_channel::Sender<core::convert::Infallible>);

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
struct Inner {
    executor: std::sync::Arc<executor_core::AnyExecutor>,
    guard: ShutdownGuard,
}

#[cfg(not(target_arch = "wasm32"))]
impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerContext").finish_non_exhaustive()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl WorkerContext {
    /// Build the context the built-in runtime hands to a request.
    #[must_use]
    pub const fn new(
        executor: std::sync::Arc<executor_core::AnyExecutor>,
        guard: ShutdownGuard,
    ) -> Self {
        Self(Inner { executor, guard })
    }

    /// Run `future` to completion after the response has been sent.
    ///
    /// The task is registered with graceful shutdown, so `Ctrl+C` drains it along with the
    /// in-flight requests instead of cutting it off.
    ///
    /// # Errors
    ///
    /// Never on this target: the runtime that provided the context can always take the task. The
    /// `Result` matches the `wasm32` signature so the same handler compiles on both.
    pub fn wait_until<F>(&self, future: F) -> Result<(), WorkerContextError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        use executor_core::Executor as _;

        // The guard clone lives inside the spawned task, not in the `WorkerContext` the request
        // extensions hold: the context is dropped with the request, and the whole point is for the
        // work to outlive it.
        let guard = self.0.guard.0.clone();
        self.0
            .executor
            .spawn(async move {
                let _guard = guard;
                future.await;
            })
            .detach();
        Ok(())
    }
}

// ── WebAssembly ──

#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
struct Inner(wasm_bindgen::JsValue);

// SAFETY: WASM in a WinterCG runtime executes on a single thread, so the JS handle never crosses
// a thread boundary. This mirrors `WasmEnv`, which holds the environment the same way.
#[cfg(target_arch = "wasm32")]
unsafe impl Send for Inner {}
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for Inner {}

#[cfg(target_arch = "wasm32")]
impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerContext").finish_non_exhaustive()
    }
}

#[cfg(target_arch = "wasm32")]
impl WorkerContext {
    /// Wrap the `ExecutionContext` a `WinterCG` `fetch` export receives as its third argument.
    #[must_use]
    pub const fn new(context: super::wasm::ExecutionContext) -> Self {
        Self(Inner(context))
    }

    /// The raw `ExecutionContext` value, for platform APIs this type does not wrap.
    #[must_use]
    pub const fn as_js(&self) -> &wasm_bindgen::JsValue {
        &self.0 .0
    }

    /// Keep the isolate alive until `future` completes, after the response has been returned.
    ///
    /// Without this the runtime is free to tear the isolate down the moment the response is sent,
    /// so an un-awaited future simply never runs.
    ///
    /// # Errors
    ///
    /// [`WorkerContextError`] when the value threaded in is not an `ExecutionContext`, or when the
    /// runtime rejects the promise.
    pub fn wait_until<F>(&self, future: F) -> Result<(), WorkerContextError>
    where
        F: Future<Output = ()> + 'static,
    {
        let promise = wasm_bindgen_futures::future_to_promise(async move {
            future.await;
            Ok(wasm_bindgen::JsValue::UNDEFINED)
        });
        self.call_method("waitUntil", Some(&promise.into()))
    }

    /// Tell the runtime to fall through to the origin if this request throws.
    ///
    /// `wasm32`-only: failing open to an origin is a Cloudflare routing behaviour with no native
    /// equivalent, so a portable handler that calls this does not compile rather than silently
    /// doing nothing off the edge.
    ///
    /// # Errors
    ///
    /// [`WorkerContextError`] when the value threaded in is not an `ExecutionContext`, or when the
    /// runtime rejects the request.
    pub fn pass_through_on_exception(&self) -> Result<(), WorkerContextError> {
        self.call_method("passThroughOnException", None)
    }

    /// Call a method on the raw `ExecutionContext`, naming it when it is not there.
    ///
    /// The context reaches the framework as an untyped `JsValue`, so a runtime that hands over
    /// something else — or an older one missing the method — is reported by name here rather than
    /// as an opaque `TypeError` from the JS side.
    fn call_method(
        &self,
        name: &str,
        argument: Option<&wasm_bindgen::JsValue>,
    ) -> Result<(), WorkerContextError> {
        use wasm_bindgen::{JsCast as _, JsValue};

        let context = &self.0 .0;
        let method = js_sys::Reflect::get(context, &JsValue::from_str(name))
            .map_err(|error| WorkerContextError::new(format!("{error:?}")))?;
        let method = method.dyn_into::<js_sys::Function>().map_err(|_| {
            WorkerContextError::new(format!(
                "the execution context has no `{name}` method; is this a WinterCG fetch handler?"
            ))
        })?;

        let called = argument.map_or_else(
            || method.call0(context),
            |argument| method.call1(context, argument),
        );
        called
            .map(|_| ())
            .map_err(|error| WorkerContextError::new(format!("{error:?}")))
    }
}
