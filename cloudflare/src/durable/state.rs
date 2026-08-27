//! Durable Object state adapter and extension injection.

use core::future::Future;

use skyzen::durable::DurableObjectState as HibernationDurableObjectState;
use skyzen::durable::{DurableConnections, DurableContext, DurableObjectError, DurableObjectId};
use skyzen_services::durable::{Alarm, DurableDb, DurableKv};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{future_to_promise, JsFuture};
use worker::send::IntoSendFuture;

use crate::ffi;

use super::{
    alarm::CfAlarm,
    kv::CfDurableKv,
    sql::CfDurableDb,
    websocket::{clone_state, CfDurableConnections, SendSyncDurableState},
};

/// How [`CfDurableState::abort_with`] resets the object.
#[derive(Debug, Clone, Copy)]
pub struct AbortOptions {
    /// Whether an alarm the reset interrupted is retried afterwards.
    ///
    /// Defaults to `true`, which is the runtime's own behaviour. Set it to `false` when the alarm
    /// is what put the object into the state being aborted, so the retry does not repeat it.
    pub retry_alarm: bool,
}

impl Default for AbortOptions {
    fn default() -> Self {
        Self { retry_alarm: true }
    }
}

/// Cloudflare Durable Object state wrapper.
pub struct CfDurableState {
    state: worker_sys::DurableObjectState,
    env: JsValue,
}

impl Clone for CfDurableState {
    fn clone(&self) -> Self {
        Self {
            state: clone_state(&self.state),
            env: self.env.clone(),
        }
    }
}

unsafe impl Send for CfDurableState {}
unsafe impl Sync for CfDurableState {}

impl std::fmt::Debug for CfDurableState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfDurableState").finish_non_exhaustive()
    }
}

impl CfDurableState {
    /// Create from raw Cloudflare `DurableObjectState`.
    #[must_use]
    pub const fn new(state: worker_sys::DurableObjectState, env: JsValue) -> Self {
        Self { state, env }
    }

    /// Clone and return the raw state handle.
    #[must_use]
    pub fn raw_state(&self) -> worker_sys::DurableObjectState {
        clone_state(&self.state)
    }

    /// Build the Durable KV extractor wrapper.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if state storage cannot be accessed.
    pub fn kv(&self) -> Result<DurableKv, DurableObjectError> {
        let store = CfDurableKv::from_state(&self.state).map_err(to_runtime_error)?;
        Ok(DurableKv::new(store))
    }

    /// Build the Durable database extractor wrapper.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if state storage cannot be accessed.
    pub fn db(&self) -> Result<DurableDb, DurableObjectError> {
        let store = CfDurableDb::from_state(&self.state).map_err(to_runtime_error)?;
        Ok(DurableDb::new(store))
    }

    /// Build the alarm extractor wrapper.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if state storage cannot be accessed.
    pub fn alarm(&self) -> Result<Alarm, DurableObjectError> {
        let scheduler = CfAlarm::from_state(&self.state).map_err(to_runtime_error)?;
        Ok(Alarm::new(scheduler))
    }

    /// Run `critical_section` with the object's input gates closed.
    ///
    /// While it runs, the runtime queues every other event — incoming fetches, alarms and
    /// websocket messages — instead of interleaving them. That is the platform's answer to two
    /// problems Skyzen's own state model makes concrete: initialization that must finish before
    /// the first request is served, and a sequence of writes that must land together even though
    /// it spans an `await`.
    ///
    /// A failure inside the critical section aborts the Durable Object, which is deliberate on
    /// Cloudflare's part: an object whose initialization failed has no safe state to serve from,
    /// so it is torn down and rebuilt rather than left half-constructed.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError::Runtime`] if the state handle does not expose
    /// `blockConcurrencyWhile` or the runtime rejects the call.
    pub async fn block_concurrency_while<F>(
        &self,
        critical_section: F,
    ) -> Result<(), DurableObjectError>
    where
        F: Future<Output = ()> + 'static,
    {
        // `once_into_js` hands ownership of the closure to JS, so nothing `!Send` is held across
        // the await below and this future stays `Send` like the rest of the Durable Object API.
        let callback = Closure::once_into_js(move || {
            future_to_promise(async move {
                critical_section.await;
                Ok(JsValue::UNDEFINED)
            })
        });

        let state: &ffi::DurableObjectStateExt = self.state.unchecked_ref();
        let promise = state
            .block_concurrency_while(callback.unchecked_ref())
            .map_err(runtime_err)?;
        JsFuture::from(promise)
            .into_send()
            .await
            .map_err(runtime_err)?;
        Ok(())
    }

    /// Hand `work` to the runtime alongside the current event.
    ///
    /// # This is not `WorkerContext::wait_until`
    ///
    /// On a plain Worker, `waitUntil` is what keeps the isolate alive past the response. On a
    /// Durable Object it does neither: Cloudflare documents the method as existing "for API
    /// compatibility with Workers Runtime APIs" and says it "has no effect in Durable Objects. It
    /// does not extend the lifetime of a Durable Object or affect when a request or RPC
    /// completes", because "Durable Objects automatically remain active as long as there is
    /// ongoing work or pending I/O".
    ///
    /// So this wrapper is for code that must call the platform method — usually because it is
    /// shared with a Worker path — and not a way to schedule background work. Work spawned from a
    /// Durable Object already outlives the response on its own.
    ///
    /// See <https://developers.cloudflare.com/durable-objects/api/state/>.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError::Runtime`] if the runtime rejects the call.
    pub fn wait_until<F>(&self, work: F) -> Result<(), DurableObjectError>
    where
        F: Future<Output = ()> + 'static,
    {
        let promise = future_to_promise(async move {
            work.await;
            Ok(JsValue::UNDEFINED)
        });
        self.state.wait_until(&promise).map_err(runtime_err)
    }

    /// Immediately reset this Durable Object, logging `reason` as the error that caused it.
    ///
    /// The runtime tears the object down and rebuilds it on the next request: in-memory state is
    /// discarded and in-flight work is abandoned. That is the point — it is the escape hatch for
    /// an object that has reached a state it cannot serve from, where continuing would serve
    /// wrong answers rather than none.
    ///
    /// Uses the platform's default alarm behaviour, under which an alarm interrupted by the abort
    /// is retried. Use [`abort_with`](Self::abort_with) to say otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError::Runtime`] if the state handle does not expose `abort` or the
    /// runtime rejects the call.
    pub fn abort(&self, reason: &str) -> Result<(), DurableObjectError> {
        let state: &ffi::DurableObjectStateExt = self.state.unchecked_ref();
        state
            .abort(reason, &JsValue::UNDEFINED)
            .map_err(runtime_err)
    }

    /// Reset this Durable Object, choosing what happens to an alarm the reset interrupts.
    ///
    /// Cloudflare's guidance is to pass `retry_alarm = false` "on any abort call that should
    /// prevent an interrupted alarm from retrying, including abort calls outside the alarm
    /// handler" — an alarm that aborted the object once will do it again on retry.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError::Runtime`] if the state handle does not expose `abort` or the
    /// runtime rejects the call.
    pub fn abort_with(
        &self,
        reason: &str,
        options: AbortOptions,
    ) -> Result<(), DurableObjectError> {
        let js_options = js_sys::Object::new();
        js_sys::Reflect::set(
            &js_options,
            &JsValue::from_str("retryAlarm"),
            &JsValue::from_bool(options.retry_alarm),
        )
        .map_err(runtime_err)?;

        let state: &ffi::DurableObjectStateExt = self.state.unchecked_ref();
        state
            .abort(reason, js_options.as_ref())
            .map_err(runtime_err)
    }

    /// Build the websocket connections extractor wrapper.
    #[must_use]
    pub fn connections(&self) -> DurableConnections {
        DurableConnections::new(Box::new(CfDurableConnections::new(clone_state(
            &self.state,
        ))))
    }

    /// Convert Cloudflare `DurableObjectId` into Skyzen `DurableObjectId`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the runtime ID cannot be read.
    pub fn id(&self) -> Result<DurableObjectId, DurableObjectError> {
        let id = self.state.id().map_err(runtime_err)?;
        let id_string = id.to_string().map_err(runtime_err)?;
        Ok(DurableObjectId::new(id_string, id.name()))
    }

    /// Build runtime context for websocket event handlers.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if any service cannot be created.
    pub fn context(&self) -> Result<DurableContext, DurableObjectError> {
        Ok(DurableContext::new(
            self.kv()?,
            self.db()?,
            self.alarm()?,
            self.connections(),
            self.id()?,
        ))
    }

    /// Inject all Durable Object services into request extensions.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if any service cannot be created.
    pub fn inject_request_extensions(
        &self,
        request: &mut skyzen::Request,
    ) -> Result<(), DurableObjectError> {
        request.extensions_mut().insert(self.kv()?);
        request.extensions_mut().insert(self.db()?);
        request.extensions_mut().insert(self.alarm()?);
        request.extensions_mut().insert(self.connections());
        request
            .extensions_mut()
            .insert(skyzen::runtime::wasm::WasmEnv::new(self.env.clone()));

        let state = SendSyncDurableState(clone_state(&self.state));
        request
            .extensions_mut()
            .insert(HibernationDurableObjectState::new(move |socket, tags| {
                if tags.is_empty() {
                    return state.0.accept_websocket(socket).map_err(websocket_err);
                }

                let tags = tags
                    .iter()
                    .map(|tag| JsValue::from_str(tag))
                    .collect::<Vec<_>>();
                state
                    .0
                    .accept_websocket_with_tags(socket, tags)
                    .map_err(websocket_err)
            }));

        Ok(())
    }
}

fn to_runtime_error(error: impl std::fmt::Display) -> DurableObjectError {
    DurableObjectError::Runtime(error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn runtime_err(error: JsValue) -> DurableObjectError {
    DurableObjectError::Runtime(format!("{error:?}"))
}

#[allow(clippy::needless_pass_by_value)]
fn websocket_err(error: JsValue) -> DurableObjectError {
    DurableObjectError::WebSocket(format!("{error:?}"))
}
