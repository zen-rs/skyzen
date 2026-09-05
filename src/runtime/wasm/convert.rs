//! The one conversion between `WinterCG` request/response objects and Skyzen's own.
//!
//! Both directions live here, and every caller goes through them: the Worker `fetch` entry point
//! in [`super::serve`], the Durable Object runtime glue, and any outbound subrequest a platform
//! crate makes (a Durable Object stub, a service binding). A second hand-rolled copy is how the
//! two sides drift — one appending repeated headers while the other overwrites them, one
//! streaming a body while the other buffers it — so there is exactly one.
//!
//! Bodies stream in both directions. Nothing here buffers, so a Durable Object can be handed a
//! multi-megabyte upload and answer with a stream without either end holding it in memory.

use std::{
    error::Error as StdError,
    fmt,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

use futures_core::Stream;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Body, BodyError, Method, StatusCode, Uri,
};

use super::{Request, Response};

/// Convert a `WinterCG` [`Request`] into a Skyzen request, streaming its body.
///
/// Cloudflare's `request.cf` is picked up when present; on a host that has no such property its
/// absence is not an error, just a request with no edge metadata to extract.
///
/// # Errors
///
/// Returns a `JsValue` error when the method, URL or a header is not something `http` accepts.
pub fn from_js_request(request: &Request) -> Result<crate::Request, JsValue> {
    let method = request
        .method()
        .parse::<Method>()
        .map_err(|error| JsValue::from_str(&format!("invalid request method: {error}")))?;
    let uri = request
        .url()
        .parse::<Uri>()
        .map_err(|error| JsValue::from_str(&format!("invalid request URI: {error}")))?;

    let mut sky_request = crate::Request::new(body_from_js_stream(request.body()));
    *sky_request.method_mut() = method;
    *sky_request.uri_mut() = uri;
    *sky_request.headers_mut() = headers_from_js(&request.headers())?;

    if let Some(slot) = crate::runtime::CfPropertiesSlot::read(request)? {
        sky_request.extensions_mut().insert(slot);
    }

    Ok(sky_request)
}

/// Render a Skyzen request as a `WinterCG` [`Request`], for an outbound subrequest.
///
/// # Errors
///
/// Returns a `JsValue` error when the URI or a header is not something the runtime accepts.
pub fn into_js_request(request: crate::Request) -> Result<Request, JsValue> {
    let (parts, body) = request.into_parts();

    let init = web_sys::RequestInit::new();
    init.set_method(parts.method.as_str());
    init.set_headers_headers(&headers_into_js(&parts.headers)?);

    // `GET` and `HEAD` may not carry a body — the `Request` constructor throws on one — and a
    // body already known to be empty has nothing to stream.
    let sends_body =
        !matches!(parts.method, Method::GET | Method::HEAD) && body.is_empty() != Some(true);
    if sends_body {
        init.set_body_opt_readable_stream(Some(&body_into_js_stream(body)));
        // A streaming request body is half-duplex: the request is written before the response is
        // read. Runtimes that require the opt-in reject the stream without it, and the rest
        // ignore an init key they do not know.
        js_sys::Reflect::set(
            init.as_ref(),
            &JsValue::from_str("duplex"),
            &JsValue::from_str("half"),
        )?;
    }

    Request::new_with_str_and_init(&parts.uri.to_string(), &init)
}

/// Convert a `WinterCG` [`Response`] into a Skyzen response, streaming its body.
///
/// A `101 Switching Protocols` answer carries its socket on the `webSocket` property rather than
/// in a body; it is moved into the response extensions so that returning the response from a
/// handler hands the socket back to the client, which is what proxying an upgrade to a Durable
/// Object amounts to.
///
/// # Errors
///
/// Returns a `JsValue` error when the status or a header is not something `http` accepts, or when
/// a `101` arrives without the socket that has to accompany it.
pub fn from_js_response(response: &Response) -> Result<crate::Response, JsValue> {
    let status = StatusCode::from_u16(response.status())
        .map_err(|error| JsValue::from_str(&format!("invalid response status: {error}")))?;

    let mut sky_response = crate::Response::new(body_from_js_stream(response.body()));
    *sky_response.status_mut() = status;
    *sky_response.headers_mut() = headers_from_js(&response.headers())?;

    if status == StatusCode::SWITCHING_PROTOCOLS {
        attach_upgraded_socket(response, &mut sky_response)?;
    }

    Ok(sky_response)
}

/// Render a Skyzen response as a `WinterCG` [`Response`], streaming its body.
///
/// # Errors
///
/// Returns a `JsValue` error when a header is not something the runtime accepts or the response
/// cannot be constructed.
pub fn into_js_response(response: crate::Response) -> Result<Response, JsValue> {
    // Only the websocket handling below needs mutable access.
    #[cfg(feature = "ws")]
    let mut response = response;

    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        #[cfg(feature = "ws")]
        {
            let socket = response
                .extensions_mut()
                .remove::<crate::durable::DurableClientWebSocket>()
                .map(|socket| JsValue::from(socket.0))
                .or_else(|| {
                    response
                        .extensions_mut()
                        .remove::<crate::websocket::SendSyncWebSocket>()
                        .map(|socket| JsValue::from(socket.into_inner()))
                });
            if let Some(socket) = socket {
                return upgrade_response(&socket, response.headers());
            }
        }

        // A 101 without an attached WebSocket cannot be represented by the standard Response
        // constructor — it throws an opaque RangeError — so name the real problem instead.
        tracing::error!("101 response without an attached WebSocket");
        return Err(JsValue::from_str(
            "101 Switching Protocols response was missing a WebSocket",
        ));
    }

    let status = response.status();
    let init = web_sys::ResponseInit::new();
    init.set_status(status.as_u16());
    init.set_status_text(status.canonical_reason().unwrap_or("OK"));
    init.set_headers_headers(&headers_into_js(response.headers())?);

    if status_forbids_body(status) {
        Response::new_with_opt_readable_stream_and_init(None, &init)
    } else {
        let body = body_into_js_stream(response.into_body());
        Response::new_with_opt_readable_stream_and_init(Some(&body), &init)
    }
}

/// The Fetch `Response` constructor rejects a body for these HTTP statuses.
const fn status_forbids_body(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::NO_CONTENT | StatusCode::RESET_CONTENT | StatusCode::NOT_MODIFIED
    )
}

/// Build the `101` response that hands a socket back to the client.
///
/// The headers are not decoration. RFC 6455 §4.1 has a client that offered a subprotocol **fail
/// the connection** when the handshake comes back without `Sec-WebSocket-Protocol`, and a browser
/// `WebSocket` can send no other custom header — so a `101` built from status and socket alone
/// makes token-over-subprotocol authentication impossible rather than merely awkward.
#[cfg(feature = "ws")]
fn upgrade_response(socket: &JsValue, headers: &HeaderMap) -> Result<Response, JsValue> {
    let init = web_sys::ResponseInit::new();
    init.set_status(StatusCode::SWITCHING_PROTOCOLS.as_u16());
    init.set_headers_headers(&headers_into_js(headers)?);
    // `web_sys::ResponseInit` has no `webSocket` field — it is a Workers/WinterCG extension — so
    // it is attached reflectively.
    js_sys::Reflect::set(init.as_ref(), &JsValue::from_str("webSocket"), socket)?;
    Response::new_with_opt_buffer_source_and_init(None, &init)
}

/// Move the socket of an incoming `101` into the Skyzen response extensions.
#[cfg(feature = "ws")]
fn attach_upgraded_socket(
    response: &Response,
    sky_response: &mut crate::Response,
) -> Result<(), JsValue> {
    let socket = js_sys::Reflect::get(response.as_ref(), &JsValue::from_str("webSocket"))?;
    if socket.is_undefined() || socket.is_null() {
        return Err(JsValue::from_str(
            "101 Switching Protocols response arrived without a `webSocket` property",
        ));
    }
    sky_response
        .extensions_mut()
        .insert(crate::durable::DurableClientWebSocket(
            socket.unchecked_into(),
        ));
    Ok(())
}

/// Without the `ws` feature there is no type to carry the socket, so a `101` cannot be forwarded.
#[cfg(not(feature = "ws"))]
fn attach_upgraded_socket(
    _response: &Response,
    _sky_response: &mut crate::Response,
) -> Result<(), JsValue> {
    Err(JsValue::from_str(
        "received a 101 Switching Protocols response; enable skyzen's `ws` feature to forward it",
    ))
}

/// Collect a JS `Headers` object into an `http` header map.
///
/// Entries are appended rather than inserted: `Set-Cookie` and friends legitimately repeat, and
/// inserting would keep only the last of them.
fn headers_from_js(headers: &web_sys::Headers) -> Result<HeaderMap, JsValue> {
    let iter = js_sys::try_iter(headers)?
        .ok_or_else(|| JsValue::from_str("Headers iterator unavailable"))?;

    let mut map = HeaderMap::new();
    for entry in iter {
        let pair = js_sys::Array::from(&entry?);
        let key = pair
            .get(0)
            .as_string()
            .ok_or_else(|| JsValue::from_str("invalid header name"))?;
        let value = pair
            .get(1)
            .as_string()
            .ok_or_else(|| JsValue::from_str("invalid header value"))?;

        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|error| {
            JsValue::from_str(&format!("failed to parse header name `{key}`: {error}"))
        })?;
        let value = HeaderValue::from_str(&value).map_err(|error| {
            JsValue::from_str(&format!(
                "failed to parse header value for `{key}`: {error}"
            ))
        })?;
        map.append(name, value);
    }
    Ok(map)
}

/// Render an `http` header map as a JS `Headers` object.
fn headers_into_js(headers: &HeaderMap) -> Result<web_sys::Headers, JsValue> {
    let js_headers = web_sys::Headers::new()?;
    for (name, value) in headers {
        // Header values are not guaranteed to be ASCII; a lossy UTF-8 view preserves them
        // instead of silently dropping the non-ASCII ones.
        js_headers.append(name.as_str(), &String::from_utf8_lossy(value.as_bytes()))?;
    }
    Ok(js_headers)
}

/// Wrap a JS body stream as a Skyzen [`Body`]; a missing stream is an empty body.
fn body_from_js_stream(stream: Option<web_sys::ReadableStream>) -> Body {
    let Some(raw_stream) = stream else {
        return Body::empty();
    };
    Body::from_stream(JsReadableBody {
        inner: wasm_streams::ReadableStream::from_raw(raw_stream).into_stream(),
    })
}

/// Expose a Skyzen [`Body`] to JS as a `ReadableStream`.
fn body_into_js_stream(body: Body) -> web_sys::ReadableStream {
    wasm_streams::ReadableStream::from_stream(BodyReadableStream { body }).into_raw()
}

#[derive(Debug)]
struct JsReadableBody {
    inner: wasm_streams::readable::IntoStream<'static>,
}

// SAFETY: WinterCG WASM body streams are confined to the single-threaded JS event loop.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for JsReadableBody {}
// SAFETY: `JsReadableBody` is only polled by the single-threaded WASM runtime.
unsafe impl Sync for JsReadableBody {}

impl Stream for JsReadableBody {
    type Item = Result<Vec<u8>, BodyError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx).map(|item| {
            item.map(|result| match result {
                Ok(value) => js_value_to_body_bytes(&value),
                Err(error) => Err(body_stream_error(js_value_to_error_message(&error))),
            })
        })
    }
}

#[derive(Debug)]
struct BodyReadableStream {
    body: Body,
}

impl Stream for BodyReadableStream {
    type Item = Result<JsValue, JsValue>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.body).poll_next(cx).map(|item| {
            item.map(|result| {
                result
                    .map(|bytes| bytes_to_js_value(&bytes))
                    .map_err(|error| body_error_to_js(&error))
            })
        })
    }
}

fn js_value_to_body_bytes(value: &JsValue) -> Result<Vec<u8>, BodyError> {
    if value.is_instance_of::<js_sys::Uint8Array>() || value.is_instance_of::<js_sys::ArrayBuffer>()
    {
        Ok(js_sys::Uint8Array::new(value).to_vec())
    } else {
        Err(body_stream_error("body stream yielded a non-byte chunk"))
    }
}

fn bytes_to_js_value(bytes: &bytes::Bytes) -> JsValue {
    js_sys::Uint8Array::from(bytes.as_ref()).into()
}

fn body_error_to_js(error: &BodyError) -> JsValue {
    JsValue::from_str(&error.to_string())
}

fn js_value_to_error_message(value: &JsValue) -> String {
    value
        .as_string()
        .unwrap_or_else(|| format!("body stream read failed: {value:?}"))
}

fn body_stream_error(message: impl Into<String>) -> BodyError {
    BodyError::Other(Box::new(WasmBodyStreamError(message.into())))
}

#[derive(Debug)]
struct WasmBodyStreamError(String);

impl fmt::Display for WasmBodyStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for WasmBodyStreamError {}

#[cfg(test)]
mod tests {
    use super::status_forbids_body;
    use crate::StatusCode;

    #[test]
    fn fetch_null_body_statuses_are_named_exhaustively() {
        assert!(status_forbids_body(StatusCode::NO_CONTENT));
        assert!(status_forbids_body(StatusCode::RESET_CONTENT));
        assert!(status_forbids_body(StatusCode::NOT_MODIFIED));
        assert!(!status_forbids_body(StatusCode::OK));
        assert!(!status_forbids_body(StatusCode::PARTIAL_CONTENT));
    }
}
