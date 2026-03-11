//! Durable Object state adapter and extension injection.

use skyzen::durable::DurableObjectState as HibernationDurableObjectState;
use skyzen::durable::{DurableConnections, DurableContext, DurableObjectError, DurableObjectId};
use skyzen_services::durable::{Alarm, DurableKv, DurableSql};
use wasm_bindgen::JsValue;

use super::{
    alarm::CfAlarm,
    kv::CfDurableKv,
    sql::CfDurableSql,
    websocket::{clone_state, CfDurableConnections},
};

struct SendSyncDurableState(worker_sys::DurableObjectState);

// SAFETY: Workers WASM executes on a single thread; JS handles are safe to mark Send/Sync.
unsafe impl Send for SendSyncDurableState {}
unsafe impl Sync for SendSyncDurableState {}

impl Clone for SendSyncDurableState {
    fn clone(&self) -> Self {
        Self(clone_state(&self.0))
    }
}

/// Cloudflare Durable Object state wrapper.
pub struct CfDurableState {
    state: worker_sys::DurableObjectState,
}

impl Clone for CfDurableState {
    fn clone(&self) -> Self {
        Self {
            state: clone_state(&self.state),
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
    pub const fn new(state: worker_sys::DurableObjectState) -> Self {
        Self { state }
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

    /// Build the Durable SQL extractor wrapper.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if state storage cannot be accessed.
    pub fn sql(&self) -> Result<DurableSql, DurableObjectError> {
        let store = CfDurableSql::from_state(&self.state).map_err(to_runtime_error)?;
        Ok(DurableSql::new(store))
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
            self.sql()?,
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
        request.extensions_mut().insert(self.sql()?);
        request.extensions_mut().insert(self.alarm()?);
        request.extensions_mut().insert(self.connections());

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
