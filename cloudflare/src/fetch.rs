//! Send-safe wrapper around Cloudflare Workers global `fetch`.
//!
//! This is the *global* fetch, for calling anything reachable over the network. To reach a sibling
//! Worker inside Cloudflare's network, use [`CfService`](crate::CfService) instead: a service
//! binding skips DNS, TLS and the public internet entirely.

use js_sys::global;
use serde::de::DeserializeOwned;
use wasm_bindgen::{JsCast, JsValue};
use worker::send::{IntoSendFuture, SendWrapper};
use worker::{Request, Response};

/// Cloudflare-specific options for an outbound subrequest — the `cf` property of `fetch`'s init.
///
/// These are how a Worker drives Cloudflare's own cache and image pipeline on the requests it
/// makes, rather than only on the ones it answers.
#[derive(Debug, Clone, Default)]
pub struct CfFetchOptions {
    /// Override the origin's cache TTL, in seconds.
    pub cache_ttl: Option<u32>,
    /// Cache the response even when its content type would normally be uncacheable.
    pub cache_everything: Option<bool>,
    /// Treat this string as the cache key instead of the URL, so several URLs share one entry.
    pub cache_key: Option<String>,
    /// Tags to purge this entry by later (Enterprise).
    pub cache_tags: Vec<String>,
    /// Resolve the request against this hostname instead of the URL's own.
    pub resolve_override: Option<String>,
    /// Image `polish` mode: `off`, `lossless`, `lossy`.
    pub polish: Option<String>,
    /// Anything else the `cf` object accepts, merged in verbatim.
    ///
    /// `image` alone is a nested object with a dozen keys and the platform keeps adding to it, so
    /// there is an escape hatch rather than a wrapper that goes stale. Must be a JSON object.
    pub extra: Option<serde_json::Value>,
}

impl CfFetchOptions {
    /// Options that ask for nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the origin's cache TTL, in seconds.
    #[must_use]
    pub const fn with_cache_ttl(mut self, seconds: u32) -> Self {
        self.cache_ttl = Some(seconds);
        self
    }

    /// Cache the response regardless of its content type.
    #[must_use]
    pub const fn with_cache_everything(mut self, cache_everything: bool) -> Self {
        self.cache_everything = Some(cache_everything);
        self
    }

    /// Use `key` as the cache key instead of the URL.
    #[must_use]
    pub fn with_cache_key(mut self, key: impl Into<String>) -> Self {
        self.cache_key = Some(key.into());
        self
    }

    /// Tag the cache entry so it can be purged by tag.
    #[must_use]
    pub fn with_cache_tag(mut self, tag: impl Into<String>) -> Self {
        self.cache_tags.push(tag.into());
        self
    }

    /// Resolve the request against `host` instead of the URL's hostname.
    #[must_use]
    pub fn with_resolve_override(mut self, host: impl Into<String>) -> Self {
        self.resolve_override = Some(host.into());
        self
    }

    /// Set the image `polish` mode.
    #[must_use]
    pub fn with_polish(mut self, polish: impl Into<String>) -> Self {
        self.polish = Some(polish.into());
        self
    }

    /// Merge additional `cf` keys in verbatim.
    #[must_use]
    pub fn with_extra(mut self, extra: serde_json::Value) -> Self {
        self.extra = Some(extra);
        self
    }

    /// Render the options as the `cf` object `fetch` takes.
    fn into_js(self) -> Result<JsValue, CfFetchError> {
        // Start from `extra` so a named option always wins over the escape hatch: two sources
        // setting the same key silently disagreeing is exactly the bug this ordering prevents.
        let cf = match self.extra {
            Some(extra) => {
                let value = serde_wasm_bindgen::to_value(&extra)
                    .map_err(|error| CfFetchError::Backend(error.to_string()))?;
                value.dyn_into::<js_sys::Object>().map_err(|_| {
                    CfFetchError::Backend("CfFetchOptions::extra must be a JSON object".to_owned())
                })?
            }
            None => js_sys::Object::new(),
        };

        if let Some(cache_ttl) = self.cache_ttl {
            set(&cf, "cacheTtl", &JsValue::from_f64(f64::from(cache_ttl)))?;
        }
        if let Some(cache_everything) = self.cache_everything {
            set(
                &cf,
                "cacheEverything",
                &JsValue::from_bool(cache_everything),
            )?;
        }
        if let Some(cache_key) = &self.cache_key {
            set(&cf, "cacheKey", &JsValue::from_str(cache_key))?;
        }
        if !self.cache_tags.is_empty() {
            let tags = js_sys::Array::new();
            for tag in &self.cache_tags {
                tags.push(&JsValue::from_str(tag));
            }
            set(&cf, "cacheTags", &tags)?;
        }
        if let Some(resolve_override) = &self.resolve_override {
            set(&cf, "resolveOverride", &JsValue::from_str(resolve_override))?;
        }
        if let Some(polish) = &self.polish {
            set(&cf, "polish", &JsValue::from_str(polish))?;
        }

        Ok(cf.into())
    }
}

fn set(object: &js_sys::Object, name: &str, value: &JsValue) -> Result<(), CfFetchError> {
    js_sys::Reflect::set(object, &JsValue::from_str(name), value).map_err(js_err)?;
    Ok(())
}

/// Cloudflare Workers global `fetch` wrapper.
#[derive(Debug, Default, Clone, Copy)]
pub struct CfFetch;

impl CfFetch {
    /// Create a new global fetch wrapper.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Dispatch a request through the Worker global `fetch`.
    ///
    /// # Errors
    ///
    /// Returns [`CfFetchError::Backend`] when the fetch or body read fails.
    pub fn request<'a>(
        &'a self,
        request: &'a Request,
    ) -> impl core::future::Future<Output = Result<Response, CfFetchError>> + Send + 'a {
        let request = SendWrapper::new(request.try_into().map_err(worker_err));
        async move {
            let request: worker::web_sys::Request = request.0?;
            let global: worker::web_sys::WorkerGlobalScope = global().unchecked_into();
            let init = worker::web_sys::RequestInit::new();
            let promise = global.fetch_with_request_and_init(&request, &init);
            let value = wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            Ok(Response::from(
                value.unchecked_into::<worker::web_sys::Response>(),
            ))
        }
    }

    /// Dispatch a request with Cloudflare's own subrequest options attached.
    ///
    /// This is the outbound half of `request.cf`: `fetch` is where a Worker asks Cloudflare to
    /// cache, key or transform a response it is fetching, and a bare `RequestInit` cannot express
    /// any of it.
    ///
    /// # Errors
    ///
    /// Returns [`CfFetchError::Backend`] when the options cannot be encoded or the fetch fails.
    pub fn request_with_cf<'a>(
        &'a self,
        request: &'a Request,
        options: CfFetchOptions,
    ) -> impl core::future::Future<Output = Result<Response, CfFetchError>> + Send + 'a {
        let request = SendWrapper::new(request.try_into().map_err(worker_err));
        let cf = SendWrapper::new(options.into_js());
        async move {
            let request: worker::web_sys::Request = request.0?;
            let cf = cf.0?;
            let global: worker::web_sys::WorkerGlobalScope = global().unchecked_into();
            let init = worker::web_sys::RequestInit::new();
            js_sys::Reflect::set(init.as_ref(), &JsValue::from_str("cf"), &cf).map_err(js_err)?;
            let promise = global.fetch_with_request_and_init(&request, &init);
            let value = wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(js_err)?;
            Ok(Response::from(
                value.unchecked_into::<worker::web_sys::Response>(),
            ))
        }
    }

    /// Dispatch a request and return the body as bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CfFetchError::Backend`] when the fetch or body read fails.
    // The explicit `impl Future + Send` return is part of the API contract.
    #[allow(clippy::manual_async_fn)]
    pub fn request_bytes<'a>(
        &'a self,
        request: &'a Request,
    ) -> impl core::future::Future<Output = Result<Vec<u8>, CfFetchError>> + Send + 'a {
        async move {
            let response = self.request(request).await?;
            read_response_bytes(response).await
        }
    }

    /// Dispatch a request and return the body as text.
    ///
    /// # Errors
    ///
    /// Returns [`CfFetchError::Backend`] when the fetch or body read fails.
    // The explicit `impl Future + Send` return is part of the API contract.
    #[allow(clippy::manual_async_fn)]
    pub fn request_text<'a>(
        &'a self,
        request: &'a Request,
    ) -> impl core::future::Future<Output = Result<String, CfFetchError>> + Send + 'a {
        async move {
            let response = self.request(request).await?;
            read_response_text(response).await
        }
    }

    /// Dispatch a request and deserialize the body as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`CfFetchError::Backend`] when the fetch or body read fails.
    // The explicit `impl Future + Send` return is part of the API contract.
    #[allow(clippy::manual_async_fn)]
    pub fn request_json<'a, T>(
        &'a self,
        request: &'a Request,
    ) -> impl core::future::Future<Output = Result<T, CfFetchError>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let response = self.request(request).await?;
            read_response_json(response).await
        }
    }
}

/// Error returned by [`CfFetch`].
#[derive(Debug)]
pub enum CfFetchError {
    /// Cloudflare Worker fetch backend error.
    Backend(String),
}

impl std::fmt::Display for CfFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CfFetchError {}

#[allow(clippy::needless_pass_by_value)]
fn js_err(error: JsValue) -> CfFetchError {
    CfFetchError::Backend(format!("{error:?}"))
}

#[allow(clippy::needless_pass_by_value)]
fn worker_err(error: worker::Error) -> CfFetchError {
    CfFetchError::Backend(error.to_string())
}

async fn read_response_bytes(response: Response) -> Result<Vec<u8>, CfFetchError> {
    let mut response = SendWrapper::new(response);
    response.0.bytes().into_send().await.map_err(worker_err)
}

async fn read_response_text(response: Response) -> Result<String, CfFetchError> {
    let mut response = SendWrapper::new(response);
    response.0.text().into_send().await.map_err(worker_err)
}

async fn read_response_json<T>(response: Response) -> Result<T, CfFetchError>
where
    T: DeserializeOwned + Send + 'static,
{
    let mut response = SendWrapper::new(response);
    response.0.json::<T>().into_send().await.map_err(worker_err)
}
