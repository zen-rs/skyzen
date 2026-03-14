//! Cloudflare Cache API wrapper.

use worker::send::{IntoSendFuture, SendWrapper};
use worker::{Request, Response};
use worker_sys::ext::CacheStorageExt;
use wasm_bindgen::JsCast;

/// Errors from Cloudflare Cache API operations.
#[derive(Debug, thiserror::Error)]
pub enum CfCacheError {
    /// The underlying Cache API returned an error.
    #[error("cloudflare cache error: {0}")]
    Backend(String),
}

/// Result of deleting a response from cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfCacheDeletionOutcome {
    /// The response existed and was deleted.
    Success,
    /// The response was not present.
    ResponseNotFound,
}

/// Cloudflare Cache API handle.
pub struct CfCache {
    inner: worker::web_sys::Cache,
}

impl Clone for CfCache {
    fn clone(&self) -> Self {
        let js: &wasm_bindgen::JsValue = self.inner.as_ref();
        Self {
            inner: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfCache {}
unsafe impl Sync for CfCache {}

impl std::fmt::Debug for CfCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfCache").finish_non_exhaustive()
    }
}

impl Default for CfCache {
    fn default() -> Self {
        let global: worker::web_sys::WorkerGlobalScope = js_sys::global().unchecked_into();
        Self {
            inner: global.caches().expect("CacheStorage unavailable").default(),
        }
    }
}

impl CfCache {
    /// Open a named cache.
    pub fn open(
        name: impl Into<String>,
    ) -> impl core::future::Future<Output = Result<Self, CfCacheError>> + Send {
        let name = name.into();
        async move {
            let global: worker::web_sys::WorkerGlobalScope = js_sys::global().unchecked_into();
            let promise = global
                .caches()
                .map_err(js_err)?
                .open(&name);
            let inner = wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            Ok(Self {
                inner: inner.unchecked_into(),
            })
        }
    }

    /// Store a response under a URL key.
    pub fn put_url(
        &self,
        url: impl Into<String>,
        response: Response,
    ) -> impl core::future::Future<Output = Result<(), CfCacheError>> + Send + '_ {
        let url = url.into();
        let response = SendWrapper::new(response);
        async move {
            let response: worker::web_sys::Response = response.0.into();
            let promise = self.inner.put_with_str(url.as_str(), &response);
            wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            Ok(())
        }
    }

    /// Store a response under a request key.
    pub fn put_request(
        &self,
        request: &Request,
        response: Response,
    ) -> impl core::future::Future<Output = Result<(), CfCacheError>> + Send + '_ {
        let request = SendWrapper::new(request.try_into().map_err(worker_err));
        let response = SendWrapper::new(response);
        async move {
            let request: worker::web_sys::Request = request.0?;
            let response: worker::web_sys::Response = response.0.into();
            let promise = self.inner.put_with_request(&request, &response);
            wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            Ok(())
        }
    }

    /// Lookup a response by URL key.
    pub fn get_url(
        &self,
        url: impl Into<String>,
        ignore_method: bool,
    ) -> impl core::future::Future<Output = Result<Option<Response>, CfCacheError>> + Send + '_ {
        let url = url.into();
        async move {
            let options = worker::web_sys::CacheQueryOptions::new();
            options.set_ignore_method(ignore_method);
            let promise = self
                .inner
                .match_with_str_and_options(url.as_str(), &options);
            let value = wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            if value.is_undefined() {
                Ok(None)
            } else {
                Ok(Some(Response::from(value.unchecked_into::<worker::web_sys::Response>())))
            }
        }
    }

    /// Lookup a response by request key.
    pub fn get_request(
        &self,
        request: &Request,
        ignore_method: bool,
    ) -> impl core::future::Future<Output = Result<Option<Response>, CfCacheError>> + Send + '_ {
        let request = SendWrapper::new(request.try_into().map_err(worker_err));
        async move {
            let request: worker::web_sys::Request = request.0?;
            let options = worker::web_sys::CacheQueryOptions::new();
            options.set_ignore_method(ignore_method);
            let promise = self
                .inner
                .match_with_request_and_options(&request, &options);
            let value = wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            if value.is_undefined() {
                Ok(None)
            } else {
                Ok(Some(Response::from(value.unchecked_into::<worker::web_sys::Response>())))
            }
        }
    }

    /// Delete a response by URL key.
    pub fn delete_url(
        &self,
        url: impl Into<String>,
        ignore_method: bool,
    ) -> impl core::future::Future<Output = Result<CfCacheDeletionOutcome, CfCacheError>> + Send + '_ {
        let url = url.into();
        async move {
            let options = worker::web_sys::CacheQueryOptions::new();
            options.set_ignore_method(ignore_method);
            let promise = self.inner.delete_with_str_and_options(url.as_str(), &options);
            let value: wasm_bindgen::JsValue = wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            Ok(if value.as_bool().unwrap_or(false) {
                CfCacheDeletionOutcome::Success
            } else {
                CfCacheDeletionOutcome::ResponseNotFound
            })
        }
    }

    /// Delete a response by request key.
    pub fn delete_request(
        &self,
        request: &Request,
        ignore_method: bool,
    ) -> impl core::future::Future<Output = Result<CfCacheDeletionOutcome, CfCacheError>> + Send + '_ {
        let request = SendWrapper::new(request.try_into().map_err(worker_err));
        async move {
            let request: worker::web_sys::Request = request.0?;
            let options = worker::web_sys::CacheQueryOptions::new();
            options.set_ignore_method(ignore_method);
            let promise = self
                .inner
                .delete_with_request_and_options(&request, &options);
            let value: wasm_bindgen::JsValue = wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            Ok(if value.as_bool().unwrap_or(false) {
                CfCacheDeletionOutcome::Success
            } else {
                CfCacheDeletionOutcome::ResponseNotFound
            })
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn js_err(error: wasm_bindgen::JsValue) -> CfCacheError {
    CfCacheError::Backend(format!("{error:?}"))
}

fn worker_err(error: worker::Error) -> CfCacheError {
    CfCacheError::Backend(error.to_string())
}
