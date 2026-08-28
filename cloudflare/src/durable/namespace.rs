//! Cloudflare Durable Object namespace and stub wrappers.

use core::future::Future;

use skyzen::durable::{DurableObjectError, DurableObjectId};
use skyzen::runtime::wasm::{from_js_response, into_js_request};
use skyzen::{Body, Method, Request, Response};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::send::{IntoSendFuture, SendWrapper};
use worker_sys::{DurableObject, DurableObjectNamespace};

/// Cloudflare Durable Object namespace binding.
pub struct CfDurableNamespace {
    namespace: DurableObjectNamespace,
}

impl_js_handle_traits!(CfDurableNamespace { namespace });

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
        // Duck-type before casting, as every other binding does: a `wrangler.toml` where a KV
        // namespace and a Durable Object namespace got swapped is named here rather than throwing
        // an opaque TypeError on the first `idFromName`.
        crate::ffi::require_methods(
            &binding,
            binding_name,
            &["idFromName", "idFromString", "newUniqueId", "get"],
        )
        .map_err(runtime_err)?;
        let namespace: DurableObjectNamespace = binding.unchecked_into();
        Ok(Self::new(namespace))
    }

    /// Restrict this namespace to a data-residency jurisdiction.
    ///
    /// Objects created through the returned namespace are stored and run only within that
    /// jurisdiction. It constrains *creation*: an object that already exists elsewhere is not
    /// moved, so a jurisdiction has to be chosen before the first request that creates the object.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError::Runtime`] when the runtime rejects the jurisdiction.
    pub fn jurisdiction(&self, jurisdiction: &CfJurisdiction) -> Result<Self, DurableObjectError> {
        let namespace: &crate::ffi::DurableObjectNamespaceExt = self.namespace.unchecked_ref();
        let restricted = namespace
            .jurisdiction(jurisdiction.as_str())
            .map_err(runtime_err)?;
        Ok(Self::new(restricted.unchecked_into()))
    }

    /// Get a stub for a named object pinned to a jurisdiction.
    ///
    /// Shorthand for [`jurisdiction`](Self::jurisdiction) followed by
    /// [`get_by_name`](Self::get_by_name).
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError::Runtime`] when the runtime rejects the jurisdiction or the
    /// stub cannot be acquired.
    pub fn get_in_jurisdiction(
        &self,
        jurisdiction: &CfJurisdiction,
        name: &str,
    ) -> Result<CfDurableObjectStub, DurableObjectError> {
        self.jurisdiction(jurisdiction)?.get_by_name(name)
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

/// A Durable Objects data-residency jurisdiction.
///
/// A newtype rather than an enum: Cloudflare adds jurisdictions over time, and an enum would
/// either go stale or need a stringly `Other` variant that defeats the point. The ones Cloudflare
/// documents today have constructors; anything newer goes through [`CfJurisdiction::new`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CfJurisdiction(String);

impl CfJurisdiction {
    /// Keep objects within the European Union.
    #[must_use]
    pub fn eu() -> Self {
        Self::new("eu")
    }

    /// Keep objects within `FedRAMP`-authorized US infrastructure.
    #[must_use]
    pub fn fedramp() -> Self {
        Self::new("fedramp")
    }

    /// Name a jurisdiction the way Cloudflare spells it.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The jurisdiction's name, as the platform expects it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A handle on one Durable Object instance.
///
/// # Why fetch and not RPC
///
/// Cloudflare's newer Durable Object API lets a caller invoke methods on the stub directly, which
/// is more ergonomic than encoding a call as a synthetic HTTP request. Reaching it from Rust means
/// exporting a **JavaScript class extending `DurableObject`** whose public methods form the RPC
/// surface, and `wasm-bindgen` emits standalone classes only — it cannot extend an imported base
/// class. So the generated Durable Object class exports `fetch`, `alarm` and the hibernation
/// websocket handlers, and `fetch` is what a stub can call. Encoding a cross-object call as a
/// request is the cost; it is a wasm-bindgen limitation rather than a design choice.
pub struct CfDurableObjectStub {
    stub: DurableObject,
}

impl_js_handle_traits!(CfDurableObjectStub { stub });

impl CfDurableObjectStub {
    /// Dispatch a request to the remote Durable Object.
    ///
    /// The request and the response are Skyzen's own — the same signature
    /// `NativeDurableObjectStub` has natively — so one handler body works against a simulated
    /// object in a test and a real one on Workers. Bodies stream in both directions; nothing is
    /// buffered on the way through.
    ///
    /// A `101` answer keeps its socket: it travels in the response extensions, so returning the
    /// response from a handler hands the client the Durable Object's end of the connection, which
    /// is all a websocket proxy route has to do.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if the request cannot be rendered for the platform, the
    /// fetch call fails, or the object answers with something that is not a response.
    // The explicit `impl Future + Send` return is part of the API contract: a handler awaiting
    // this must stay `Send`, which is what makes the `SendFuture` wrapper unnecessary.
    #[allow(clippy::manual_async_fn)]
    pub fn fetch(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, DurableObjectError>> + Send + '_ {
        async move {
            // The JS request lives and dies inside `dispatch`, so the only thing crossing the
            // suspension point is the promise — wrapped, because a `Promise` is not `Send`.
            let promise = SendWrapper::new(self.dispatch(request));
            let value = JsFuture::from(promise.0?)
                .into_send()
                .await
                .map_err(runtime_err)?;
            let response: web_sys::Response = value.dyn_into().map_err(|value| {
                DurableObjectError::Runtime(format!(
                    "DurableObject.fetch returned non-Response value: {value:?}"
                ))
            })?;
            from_js_response(&response).map_err(runtime_err)
        }
    }

    /// Dispatch a `GET` to the remote Durable Object by URL.
    ///
    /// # Errors
    ///
    /// Returns [`DurableObjectError`] if `url` is not a valid URI, the fetch call fails, or the
    /// object answers with something that is not a response.
    // The explicit `impl Future + Send` return is part of the API contract.
    #[allow(clippy::manual_async_fn)]
    pub fn fetch_url<'a>(
        &'a self,
        url: &'a str,
    ) -> impl Future<Output = Result<Response, DurableObjectError>> + Send + 'a {
        async move {
            let mut request = Request::new(Body::empty());
            *request.method_mut() = Method::GET;
            *request.uri_mut() = url.parse().map_err(|error| {
                DurableObjectError::Runtime(format!("invalid durable URL `{url}`: {error}"))
            })?;
            self.fetch(request).await
        }
    }

    /// Render the request for the platform and start the subrequest.
    ///
    /// Kept separate so no JS handle is ever a local of the returned future: the `web_sys`
    /// request is dropped here, and only the promise travels back.
    fn dispatch(&self, request: Request) -> Result<js_sys::Promise, DurableObjectError> {
        let request = into_js_request(request).map_err(runtime_err)?;
        self.stub.fetch_with_request(&request).map_err(runtime_err)
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
