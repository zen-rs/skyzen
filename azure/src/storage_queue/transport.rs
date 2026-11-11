//! The queue client's `azure_core` [`HttpClient`], on zenwave's native backend.
//!
//! `azure_core` sends every request through the [`Transport`] in its client options; its default is
//! reqwest on tokio. [`ZenwaveHttpClient`] answers it through zenwave's native backend instead, an
//! [`AzureHttp`], so the queue's requests need no tokio context.

use std::{collections::HashMap, sync::Arc};

use http::Method;

use async_trait::async_trait;
use azure_core::{
    error::ErrorKind,
    http::{
        headers::{HeaderName, HeaderValue, Headers},
        response::PinnedStream,
        AsyncRawResponse, Body as AzureBody, HttpClient, Method as AzureMethod,
        Request as AzureRequest, Transport,
    },
    Error, Result,
};
use futures_util::TryStreamExt as _;
use zenwave::{header, Body, Request};

use crate::AzureHttp;

/// An `azure_core` [`HttpClient`] sending its requests through an [`AzureHttp`].
#[derive(Debug)]
pub(super) struct ZenwaveHttpClient {
    http: AzureHttp,
}

impl ZenwaveHttpClient {
    /// The `azure_core` transport that sends its requests through `http`.
    pub(super) fn transport(http: AzureHttp) -> Transport {
        Transport::new(Arc::new(Self { http }))
    }
}

#[async_trait]
impl HttpClient for ZenwaveHttpClient {
    async fn execute_request(&self, request: &AzureRequest) -> Result<AsyncRawResponse> {
        let mut outgoing = zenwave_request(request)?;
        // azure_core reads an error status out of the response: its retry policy decides on the
        // status, and its error type on the body.
        let response = self.http.send(&mut outgoing).await.map_err(|error| {
            let kind = if error.is_transient() {
                ErrorKind::Io
            } else {
                ErrorKind::Connection
            };
            Error::with_error(kind, error, "the request to Azure Storage failed")
        })?;

        let (parts, body) = response.into_parts();
        let headers = parts
            .headers
            .iter()
            .map(|(name, value)| {
                let value = value.to_str().map_err(|error| {
                    Error::with_error(
                        ErrorKind::DataConversion,
                        error,
                        format!("the {name} header of an Azure Storage response is not text"),
                    )
                })?;
                Ok((
                    HeaderName::from(name.as_str().to_owned()),
                    HeaderValue::from(value.to_owned()),
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let stream: PinnedStream = Box::pin(body.into_data_stream().map_err(|error| {
            Error::with_error(
                ErrorKind::Io,
                error,
                "reading an Azure Storage response body failed",
            )
        }));
        Ok(AsyncRawResponse::new(
            parts.status.as_u16().into(),
            Headers::from(headers),
            stream,
        ))
    }
}

/// `request` as zenwave sends it.
fn zenwave_request(request: &AzureRequest) -> Result<Request> {
    let mut builder = http::Request::builder()
        .method(method(request.method())?)
        .uri(request.url().as_str());
    for (name, value) in request.headers().iter() {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let body = match request.body().clone() {
        AzureBody::Bytes(bytes) => Body::from_bytes(bytes),
        AzureBody::SeekableStream(stream) => {
            let length = stream.len();
            let body = Body::from_stream(stream.map_err(std::io::Error::other));
            if let Some(length) = length {
                builder = builder.header(header::CONTENT_LENGTH, length);
            }
            body
        }
    };
    builder.body(body).map_err(|error| {
        Error::with_error(
            ErrorKind::Other,
            error,
            "an Azure Storage request could not be built",
        )
    })
}

/// `azure_core`'s method as the `http` crate names it, by its canonical name.
fn method(method: AzureMethod) -> Result<Method> {
    Method::from_bytes(method.as_str().as_bytes()).map_err(|error| {
        Error::with_error(
            ErrorKind::Other,
            error,
            format!("{method} is not an HTTP method"),
        )
    })
}
