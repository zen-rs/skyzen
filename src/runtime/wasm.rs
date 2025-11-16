use std::future::Future;

use crate::{Body, Endpoint};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Alias matching the WinterCG request object.
pub type Request = web_sys::Request;
/// Alias for the WinterCG response object.
pub type Response = web_sys::Response;
/// Alias for arbitrary environment bindings.
pub type Env = JsValue;
/// Alias for the execution context value.
pub type ExecutionContext = JsValue;

/// Bridge the annotated endpoint into the WinterCG `fetch` contract.
pub async fn launch<Fut, E>(
    factory: impl FnOnce() -> Fut,
    request: Request,
    env: Env,
    ctx: ExecutionContext,
) -> Result<Response, JsValue>
where
    Fut: Future<Output = E>,
    E: Endpoint + Clone + 'static,
{
    let endpoint = factory().await;
    serve(endpoint, request, env, ctx).await
}

async fn serve<E>(
    mut endpoint: E,
    request: Request,
    _env: Env,
    _ctx: ExecutionContext,
) -> Result<Response, JsValue>
where
    E: Endpoint + Clone + 'static,
{
    let mut sky_request = convert_request(request).await?;
    let response = endpoint
        .respond(&mut sky_request)
        .await
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    convert_response(response).await
}

async fn convert_request(request: Request) -> Result<crate::Request, JsValue> {
    let mut builder = http::Request::builder()
        .method(request.method())
        .uri(request.url());

    let headers = request.headers();
    let iter = js_sys::try_iter(&headers)?
        .ok_or_else(|| JsValue::from_str("Headers iterator unavailable"))?;

    for entry in iter {
        let entry = entry?;
        let pair = js_sys::Array::from(&entry);
        let key = pair
            .get(0)
            .as_string()
            .ok_or_else(|| JsValue::from_str("Invalid header name"))?;
        let value = pair
            .get(1)
            .as_string()
            .ok_or_else(|| JsValue::from_str("Invalid header value"))?;
        builder = builder.header(key, value);
    }

    let bytes = read_body_bytes(&request).await?;
    let http_request = builder
        .body(Body::from(bytes))
        .map_err(|error| JsValue::from_str(&format!("Failed to build request: {error}")))?;
    Ok(crate::Request::from(http_request))
}

async fn convert_response(response: crate::Response) -> Result<Response, JsValue> {
    let status = response.status().as_u16();
    let mut init = web_sys::ResponseInit::new();
    init.status(status);
    init.status_text(response.status().canonical_reason().unwrap_or("OK"));

    let headers = web_sys::Headers::new()?;
    for (key, value) in response.headers().iter() {
        headers.append(key.as_str(), value.to_str().unwrap_or_default())?;
    }
    init.headers(&headers);

    let bytes = response
        .into_body()
        .into_bytes()
        .await
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    let array = js_sys::Uint8Array::from(bytes.as_ref());

    Response::new_with_u8_array_and_init(&array, &init)
}

async fn read_body_bytes(request: &Request) -> Result<Vec<u8>, JsValue> {
    let promise = request
        .array_buffer()
        .map_err(|error| JsValue::from(error))?;
    let buffer = JsFuture::from(promise).await?;
    let array = js_sys::Uint8Array::new(&buffer);
    Ok(array.to_vec())
}
