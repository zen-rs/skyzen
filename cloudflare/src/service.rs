//! Cloudflare service bindings — Worker-to-Worker calls over `env.BINDING.fetch`.
//!
//! A service binding routes a request to a sibling Worker inside Cloudflare's network: no DNS, no
//! TLS handshake, no public URL, and no extra request charge. It is how a multi-Worker system is
//! composed.
//!
//! # Why not RPC
//!
//! Cloudflare documents two forms of service binding: HTTP, wrapped here, and RPC through a
//! `WorkerEntrypoint` subclass. RPC is out of scope, and not by preference: the callee side is a
//! **JavaScript class extending `WorkerEntrypoint`** whose public methods become the RPC surface,
//! and `wasm-bindgen` cannot emit a class that extends an imported base class — `#[wasm_bindgen]`
//! generates standalone classes only. Exporting one would mean hand-writing JS glue per method in
//! the worker shim, which is the boilerplate a framework is supposed to remove rather than move.
//! The HTTP form is also the portable one: the same handler answers a service binding, a public
//! route and a local test.

use serde::de::DeserializeOwned;
use wasm_bindgen::{JsCast, JsValue};
use worker::send::{IntoSendFuture, SendWrapper};
use worker::{Request, Response};
use worker_sys::Fetcher;

/// Errors raised when talking to a bound service.
#[derive(Debug, thiserror::Error)]
pub enum CfServiceError {
    /// The binding is missing, or is not a service binding.
    #[error("Cloudflare service binding `{binding}` is unusable: {message}")]
    Binding {
        /// The binding name that was looked up.
        binding: String,
        /// What went wrong.
        message: String,
    },
    /// The bound Worker could not be reached, or its response could not be read.
    #[error("service binding request failed: {0}")]
    Fetch(String),
}

/// A bound sibling Worker, reachable over HTTP.
///
/// # Example
///
/// ```ignore
/// let auth = CfService::from_env(&env, "AUTH")?;
/// let request = skyzen_cloudflare::bare_request(
///     worker::Method::Get,
///     "https://auth/verify",
///     &[("Authorization", token)],
///     None,
/// )?;
/// let claims: Claims = auth.fetch_json(&request).await?;
/// ```
pub struct CfService {
    fetcher: Fetcher,
}

impl_js_handle_traits!(CfService { fetcher });

impl CfService {
    /// Wrap a raw service binding.
    ///
    /// The binding is not validated here; prefer [`CfService::from_env`], which checks that it
    /// looks like a fetcher before handing back a handle.
    #[must_use]
    pub fn new(binding: JsValue) -> Self {
        Self {
            fetcher: binding.unchecked_into(),
        }
    }

    /// Resolve a service binding from the Workers env by name.
    ///
    /// # Errors
    ///
    /// Returns [`CfServiceError::Binding`] when the binding is absent or does not expose `fetch` —
    /// the realistic failure being a `wrangler.toml` where the binding name and the service it
    /// points at have drifted apart.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, CfServiceError> {
        let binding = crate::ffi::get_binding(env, binding_name).map_err(|error| {
            CfServiceError::Binding {
                binding: binding_name.to_owned(),
                message: format!("{error:?}"),
            }
        })?;
        crate::ffi::require_methods(&binding, binding_name, &["fetch"]).map_err(|error| {
            CfServiceError::Binding {
                binding: binding_name.to_owned(),
                message: format!("{error:?}"),
            }
        })?;
        Ok(Self::new(binding))
    }

    /// Dispatch a request to the bound Worker.
    ///
    /// Build the request with [`bare_request`](crate::bare_request) or
    /// [`json_request`](crate::json_request); the URL's host is ignored by the platform, since the
    /// binding already decides which Worker answers.
    ///
    /// # Errors
    ///
    /// Returns [`CfServiceError::Fetch`] when the request cannot be converted, the bound Worker
    /// cannot be reached, or it returns something that is not a response.
    pub fn fetch<'a>(
        &'a self,
        request: &'a Request,
    ) -> impl core::future::Future<Output = Result<Response, CfServiceError>> + Send + 'a {
        let request = SendWrapper::new(request.try_into().map_err(worker_err));
        async move {
            let request: worker::web_sys::Request = request.0?;
            let promise = self.fetcher.fetch(&request).map_err(js_err)?;
            let value = wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            Ok(Response::from(
                value.unchecked_into::<worker::web_sys::Response>(),
            ))
        }
    }

    /// Dispatch a request and deserialize the response body as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`CfServiceError::Fetch`] when the call fails or the body does not deserialize.
    // The explicit `impl Future + Send` return is part of the API contract.
    #[allow(clippy::manual_async_fn)]
    pub fn fetch_json<'a, T>(
        &'a self,
        request: &'a Request,
    ) -> impl core::future::Future<Output = Result<T, CfServiceError>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let response = self.fetch(request).await?;
            let mut response = SendWrapper::new(response);
            response.0.json::<T>().into_send().await.map_err(worker_err)
        }
    }
}

/// Cloudflare's Static Assets binding.
///
/// `[assets]` in `wrangler.toml` uploads a directory to Cloudflare's asset storage and, with a
/// `binding` set, exposes it to the Worker as a fetcher. Serving a frontend through it keeps the
/// files out of the wasm module — which competes with the Worker size limit — and lets Cloudflare's
/// own asset delivery answer the request.
///
/// [`EmbeddedStaticDir`](skyzen::EmbeddedStaticDir) remains the alternative for a small payload
/// that genuinely belongs inside the binary; `StaticDir` is not an option on Workers, since there
/// is no filesystem.
///
/// The binding is a plain fetcher, so this is a [`CfService`] with a name that says what it is
/// for.
pub struct CfAssets(CfService);

impl Clone for CfAssets {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl ::std::fmt::Debug for CfAssets {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        f.debug_struct("CfAssets").finish_non_exhaustive()
    }
}

impl CfAssets {
    /// Resolve the assets binding from the Workers env by name.
    ///
    /// # Errors
    ///
    /// Returns [`CfServiceError::Binding`] when the binding is absent or is not a fetcher — the
    /// usual cause being an `[assets]` table without a `binding` key, which uploads the files but
    /// gives the Worker no handle on them.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, CfServiceError> {
        CfService::from_env(env, binding_name).map(Self)
    }

    /// Fetch the asset for `path` and render it as a Skyzen response.
    ///
    /// The status, headers and body come straight from Cloudflare's asset delivery, so a miss is a
    /// 404 from the platform and the `Content-Type`, `ETag` and cache headers are the ones it
    /// would have served directly.
    ///
    /// # Errors
    ///
    /// Returns [`CfServiceError::Fetch`] when the asset cannot be fetched or its body cannot be
    /// read.
    pub async fn respond(&self, path: &str) -> Result<skyzen::Response, CfServiceError> {
        // The host is ignored: the binding already decides what answers. A fixed one keeps the URL
        // valid without inventing a domain the user does not own.
        let url = format!("https://assets.invalid{path}");
        let request = crate::http_request::bare_request(worker::Method::Get, &url, &[], None)
            .map_err(|error| CfServiceError::Fetch(error.to_string()))?;
        let response = self.0.fetch(&request).await?;

        // Status, headers and body come straight through the framework's own conversion, so an
        // asset is rendered exactly the way an inbound response is anywhere else — and its body
        // streams rather than being buffered into the wasm heap on the way past.
        let response = SendWrapper::new(worker::web_sys::Response::from(response));
        skyzen::runtime::wasm::from_js_response(&response.0)
            .map_err(|error| CfServiceError::Fetch(format!("{error:?}")))
    }

    /// The underlying fetcher, for requests this wrapper does not build.
    #[must_use]
    pub const fn as_service(&self) -> &CfService {
        &self.0
    }
}

#[allow(clippy::needless_pass_by_value)]
fn js_err(error: JsValue) -> CfServiceError {
    CfServiceError::Fetch(format!("{error:?}"))
}

#[allow(clippy::needless_pass_by_value)]
fn worker_err(error: worker::Error) -> CfServiceError {
    CfServiceError::Fetch(error.to_string())
}
