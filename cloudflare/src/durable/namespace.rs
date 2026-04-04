//! Cloudflare Durable Object namespace and stub wrappers.

use skyzen::durable::{DurableObjectError, DurableObjectId};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::send::{IntoSendFuture, SendWrapper};
use worker_sys::{DurableObject, DurableObjectNamespace};

/// Cloudflare Durable Object namespace binding.
pub struct CfDurableNamespace {
    namespace: DurableObjectNamespace,
}

impl Clone for CfDurableNamespace {
    fn clone(&self) -> Self {
        let js: &JsValue = self.namespace.as_ref();
        Self {
            namespace: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfDurableNamespace {}
unsafe impl Sync for CfDurableNamespace {}

impl std::fmt::Debug for CfDurableNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfDurableNamespace").finish_non_exhaustive()
    }
}

impl CfDurableNamespace {
    /// Create from a raw namespace binding.
    #[must_use]
    pub const fn new(namespace: DurableObjectNamespace) -> Self {
        Self { namespace }
    }

    /// Create from a Workers environment binding name.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the binding is missing or invalid.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, DurableObjectError> {
        let binding = crate::ffi::get_binding(env, binding_name).map_err(|error| {
            DurableObjectError::Runtime(format!(
                "failed to get Durable Object binding '{binding_name}': {error:?}"
            ))
        })?;
        let namespace: DurableObjectNamespace = binding.unchecked_into();
        Ok(Self::new(namespace))
    }

    /// Resolve object ID from deterministic name.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when the runtime rejects the name.
    pub fn id_from_name(&self, name: &str) -> Result<DurableObjectId, DurableObjectError> {
        let id = self.namespace.id_from_name(name).map_err(runtime_err)?;
        to_skyzen_id(&id)
    }

    /// Resolve object ID from serialized string representation.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when the runtime rejects the string.
    pub fn id_from_string(&self, id: &str) -> Result<DurableObjectId, DurableObjectError> {
        let id = self.namespace.id_from_string(id).map_err(runtime_err)?;
        to_skyzen_id(&id)
    }

    /// Create a new random object ID.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when runtime ID generation fails.
    pub fn new_unique_id(&self) -> Result<DurableObjectId, DurableObjectError> {
        let id = self.namespace.new_unique_id().map_err(runtime_err)?;
        to_skyzen_id(&id)
    }

    /// Get a stub by object ID.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when stub acquisition fails.
    pub fn get(&self, id: &str) -> Result<CfDurableObjectStub, DurableObjectError> {
        let cf_id = self.namespace.id_from_string(id).map_err(runtime_err)?;
        let stub = self.namespace.get(&cf_id).map_err(runtime_err)?;
        Ok(CfDurableObjectStub { stub })
    }

    /// Get a stub by deterministic name.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] when stub acquisition fails.
    pub fn get_by_name(&self, name: &str) -> Result<CfDurableObjectStub, DurableObjectError> {
        let stub = self.namespace.get_by_name(name).map_err(runtime_err)?;
        Ok(CfDurableObjectStub { stub })
    }
}

/// Cloudflare Durable Object stub wrapper.
pub struct CfDurableObjectStub {
    stub: DurableObject,
}

impl Clone for CfDurableObjectStub {
    fn clone(&self) -> Self {
        let js: &JsValue = self.stub.as_ref();
        Self {
            stub: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfDurableObjectStub {}
unsafe impl Sync for CfDurableObjectStub {}

impl std::fmt::Debug for CfDurableObjectStub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfDurableObjectStub")
            .finish_non_exhaustive()
    }
}

impl CfDurableObjectStub {
    /// Dispatch request to the remote Durable Object.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the fetch call fails or returns an invalid response.
    pub fn fetch(
        &self,
        request: &web_sys::Request,
    ) -> impl core::future::Future<Output = Result<web_sys::Response, DurableObjectError>> + Send + '_
    {
        let request = SendWrapper::new(request.clone().map_err(runtime_err));
        async move {
            let request = request.0?;
            let promise = self
                .stub
                .fetch_with_request(&request)
                .map_err(runtime_err)?;
            let value = JsFuture::from(promise)
                .into_send()
                .await
                .map_err(runtime_err)?;
            value.dyn_into().map_err(|value| {
                DurableObjectError::Runtime(format!(
                    "DurableObject.fetch returned non-Response value: {value:?}"
                ))
            })
        }
    }

    /// Dispatch request to the remote Durable Object by URL string.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the fetch call fails or returns an invalid response.
    pub fn fetch_url(
        &self,
        url: &str,
    ) -> impl core::future::Future<Output = Result<web_sys::Response, DurableObjectError>> + Send + '_
    {
        let url = url.to_owned();
        async move {
            let promise = self.stub.fetch_with_str(&url).map_err(runtime_err)?;
            let value = JsFuture::from(promise)
                .into_send()
                .await
                .map_err(runtime_err)?;
            value.dyn_into().map_err(|value| {
                DurableObjectError::Runtime(format!(
                    "DurableObject.fetch returned non-Response value: {value:?}"
                ))
            })
        }
    }
}

fn to_skyzen_id(id: &worker_sys::DurableObjectId) -> Result<DurableObjectId, DurableObjectError> {
    let id_string = id.to_string().map_err(runtime_err)?;
    Ok(DurableObjectId::new(id_string, id.name()))
}

#[allow(clippy::needless_pass_by_value)]
fn runtime_err(error: JsValue) -> DurableObjectError {
    DurableObjectError::Runtime(format!("{error:?}"))
}
