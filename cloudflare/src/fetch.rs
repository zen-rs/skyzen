//! Send-safe wrapper around Cloudflare Workers global `fetch`.

use js_sys::global;
use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use worker::send::{IntoSendFuture, SendWrapper};
use worker::{Request, Response};

/// Cloudflare Workers global `fetch` wrapper.
#[derive(Debug, Default, Clone, Copy)]
pub struct CfFetch;

impl CfFetch {
    /// Create a new global fetch wrapper.
    pub const fn new() -> Self {
        Self
    }

    /// Dispatch a request through the Worker global `fetch`.
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

    /// Dispatch a request and return the body as bytes.
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

fn js_err(error: wasm_bindgen::JsValue) -> CfFetchError {
    CfFetchError::Backend(format!("{error:?}"))
}

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
