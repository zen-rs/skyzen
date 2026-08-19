//! Helpers for building Cloudflare Workers `worker::Request` instances.
//!
//! The raw `worker` API requires several manual steps to construct an HTTP
//! request: instantiate `Headers`, set each entry, build a `RequestInit`,
//! attach the body as a `Uint8Array`, and finally construct the `Request`.
//! Each step returns a `worker::Error`, so call sites end up sprinkled with
//! `.map_err(|e| MyError::X(e.to_string()))` repeatedly.
//!
//! [`json_request`] and [`bare_request`] consolidate that boilerplate behind
//! a typed [`CfHttpRequestError`]. Use [`json_request`] for outbound POSTs
//! that carry a JSON body and [`bare_request`] for GETs / HEADs / etc. that
//! don't.

const APPLICATION_JSON: &str = "application/json";

/// Errors returned when constructing a `worker::Request`.
#[derive(Debug, thiserror::Error)]
pub enum CfHttpRequestError {
    /// A header could not be inserted into the `Headers` map.
    #[error("set header `{name}`: {message}")]
    SetHeader {
        /// Header name that failed to set.
        name: String,
        /// Underlying worker error, stringified.
        message: String,
    },
    /// The JSON payload could not be serialized.
    #[error("serialize JSON body: {0}")]
    SerializeBody(String),
    /// `worker::Request::new_with_init` rejected the URL or init.
    #[error("build worker request: {0}")]
    BuildRequest(String),
}

/// Build a `worker::Request` carrying a serialized-as-JSON body.
///
/// `Content-Type: application/json` is set unless `extra_headers` already
/// carries a `Content-Type`; other `extra_headers` (for `Authorization`,
/// `Accept`, `User-Agent`, etc.) are added as-is.
///
/// # Errors
///
/// Returns [`CfHttpRequestError`] when the payload cannot be serialized, a
/// header cannot be added, or the request cannot be constructed.
pub fn json_request(
    method: worker::Method,
    url: &str,
    payload: &(impl serde::Serialize + ?Sized),
    extra_headers: &[(&str, &str)],
) -> Result<worker::Request, CfHttpRequestError> {
    let body = serde_json::to_vec(payload)
        .map_err(|error| CfHttpRequestError::SerializeBody(error.to_string()))?;
    let mut headers: Vec<(&str, &str)> = Vec::with_capacity(extra_headers.len() + 1);
    headers.extend_from_slice(extra_headers);
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
    {
        headers.push(("Content-Type", APPLICATION_JSON));
    }
    bare_request(method, url, &headers, Some(&body))
}

/// Build a `worker::Request` with optional raw bytes body.
///
/// Callers supply every header they need; nothing is added implicitly.
///
/// # Errors
///
/// Returns [`CfHttpRequestError`] when a header cannot be added or the
/// request cannot be constructed.
pub fn bare_request(
    method: worker::Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<worker::Request, CfHttpRequestError> {
    let header_map = worker::Headers::new();
    for (name, value) in headers {
        // `append` keeps duplicate header names (e.g. repeated `Set-Cookie`
        // or `Accept` entries); `set` would overwrite earlier values.
        header_map
            .append(name, value)
            .map_err(|error| CfHttpRequestError::SetHeader {
                name: (*name).to_owned(),
                message: error.to_string(),
            })?;
    }
    let mut init = worker::RequestInit::new();
    init.with_method(method);
    init.with_headers(header_map);
    if let Some(body) = body {
        let bytes = js_sys::Uint8Array::from(body);
        init.with_body(Some(wasm_bindgen::JsValue::from(bytes)));
    }
    worker::Request::new_with_init(url, &init)
        .map_err(|error| CfHttpRequestError::BuildRequest(error.to_string()))
}
