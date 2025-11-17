//! WebSocket upgrader and helpers.
//!
//! Handlers can request a protocol switch by extracting [`WebSocketUpgrade`]
//! and returning the result of [`WebSocketUpgrade::on_upgrade`]:
//! ```
//! use futures_util::{SinkExt, StreamExt};
//! use skyzen::{websocket::{WebSocketMessage, WebSocketUpgrade}, Responder};
//!
//! async fn ws_handler(ws: WebSocketUpgrade) -> impl Responder {
//!     ws.on_upgrade(|mut socket| async move {
//!         while let Some(Ok(message)) = socket.next().await {
//!             if message.is_text() {
//!                 let reply = WebSocketMessage::text(message.into_text().unwrap());
//!                 let _ = socket.send(reply).await;
//!             }
//!         }
//!     })
//! }
//! ```

use crate::{header, Method, Request, Response, Result, StatusCode};
use async_tungstenite::{
    tokio::TokioAdapter,
    tungstenite::{
        protocol::{Role, WebSocketConfig},
        Message,
    },
    WebSocketStream,
};
use http_kit::Error;
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use skyzen_core::{Extractor, Responder};
use tokio::task;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn bad_request(message: &str) -> Error {
    Error::msg(message.to_string()).set_status(StatusCode::BAD_REQUEST)
}

fn missing_upgrade() -> Error {
    Error::msg("WebSocket upgrades are not supported on this transport")
        .set_status(StatusCode::UPGRADE_REQUIRED)
}

fn header_has_token(value: &header::HeaderValue, token: &str) -> bool {
    value
        .to_str()
        .map(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
        .unwrap_or(false)
}

fn parse_protocols(value: Option<&header::HeaderValue>) -> Vec<String> {
    value
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn compute_accept_header(key: &header::HeaderValue) -> Result<header::HeaderValue> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(GUID.as_bytes());
    let digest = hasher.finalize();
    let encoded = STANDARD.encode(digest);
    header::HeaderValue::from_str(&encoded).map_err(|_| {
        Error::msg("Failed to encode websocket accept key")
            .set_status(StatusCode::INTERNAL_SERVER_ERROR)
    })
}

/// Stream representing a WebSocket connection handled by `async-tungstenite`.
pub type WebSocket = WebSocketStream<TokioAdapter<TokioIo<Upgraded>>>;

/// Convenience alias for tungstenite messages.
pub type WebSocketMessage = Message;

/// Helper that contains the state required to accept a WebSocket connection.
#[derive(Debug)]
pub struct WebSocketUpgrade {
    key: header::HeaderValue,
    on_upgrade: OnUpgrade,
    requested_protocols: Vec<String>,
    response_protocol: Option<String>,
    config: Option<WebSocketConfig>,
}

impl WebSocketUpgrade {
    /// Negotiate the sub-protocol returned to the client.
    #[must_use]
    pub fn protocols<I, S>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let supported: Vec<String> = protocols
            .into_iter()
            .map(|protocol| protocol.as_ref().to_string())
            .collect();

        self.response_protocol = self.requested_protocols.iter().find_map(|requested| {
            supported
                .iter()
                .find(|supported| *supported == requested)
                .cloned()
                .map(|_| requested.clone())
        });
        self
    }

    /// Override the [`WebSocketConfig`] used for the upgraded stream.
    #[must_use]
    pub const fn config(mut self, config: WebSocketConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Finalize the handshake and start handling the upgraded socket with `callback`.
    pub fn on_upgrade<F, Fut>(self, callback: F) -> WebSocketUpgradeResponder
    where
        F: FnOnce(WebSocket) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        WebSocketUpgradeResponder {
            upgrade: self,
            callback: Some(Box::new(move |socket| {
                Box::pin(callback(socket)) as WebSocketCallbackFuture
            })),
        }
    }
}

type WebSocketCallbackFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
type DynCallback = Box<dyn FnOnce(WebSocket) -> WebSocketCallbackFuture + Send + Sync>;

impl Extractor for WebSocketUpgrade {
    async fn extract(request: &mut Request) -> Result<Self> {
        if request.method() != Method::GET {
            return Err(Error::msg("WebSocket connections must use GET")
                .set_status(StatusCode::METHOD_NOT_ALLOWED));
        }
        let (key, requested_protocols) = {
            let headers = request.headers();

            let key = headers
                .get(header::SEC_WEBSOCKET_KEY)
                .ok_or_else(|| bad_request("Missing Sec-WebSocket-Key header"))?
                .clone();

            let connection = headers
                .get(header::CONNECTION)
                .ok_or_else(|| bad_request("Missing Connection header"))?;

            if !header_has_token(connection, "upgrade") {
                return Err(bad_request(
                    "Invalid Connection header for WebSocket request",
                ));
            }

            let upgrade_header = headers
                .get(header::UPGRADE)
                .ok_or_else(|| bad_request("Missing Upgrade header"))?;

            if !upgrade_header
                .to_str()
                .map(|value| value.eq_ignore_ascii_case("websocket"))
                .unwrap_or(false)
            {
                return Err(bad_request("Upgrade header must be `websocket`"));
            }

            match headers.get(header::SEC_WEBSOCKET_VERSION) {
                Some(version) if version == "13" => {}
                _ => {
                    return Err(bad_request(
                        "Unsupported Sec-WebSocket-Version. Only version 13 is accepted",
                    ));
                }
            }

            let requested_protocols = parse_protocols(headers.get(header::SEC_WEBSOCKET_PROTOCOL));

            (key, requested_protocols)
        };

        let on_upgrade = request
            .extensions_mut()
            .remove::<OnUpgrade>()
            .ok_or_else(missing_upgrade)?;

        Ok(Self {
            key,
            on_upgrade,
            requested_protocols,
            response_protocol: None,
            config: None,
        })
    }
}

/// [`Responder`] returned from [`WebSocketUpgrade::on_upgrade`].
pub struct WebSocketUpgradeResponder {
    upgrade: WebSocketUpgrade,
    callback: Option<DynCallback>,
}

impl std::fmt::Debug for WebSocketUpgradeResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketUpgradeResponder")
            .field("response_protocol", &self.upgrade.response_protocol)
            .field("has_callback", &self.callback.is_some())
            .finish()
    }
}

impl Responder for WebSocketUpgradeResponder {
    fn respond_to(mut self, _request: &Request, response: &mut Response) -> Result<()> {
        let accept = compute_accept_header(&self.upgrade.key)?;
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;

        {
            let headers = response.headers_mut();
            headers.insert(
                header::CONNECTION,
                header::HeaderValue::from_static("upgrade"),
            );
            headers.insert(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            );
            headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);

            if let Some(protocol) = &self.upgrade.response_protocol {
                if let Ok(value) = header::HeaderValue::from_str(protocol) {
                    headers.insert(header::SEC_WEBSOCKET_PROTOCOL, value);
                }
            }
        }

        if let Some(callback) = self.callback.take() {
            let on_upgrade = self.upgrade.on_upgrade.clone();
            let config = self.upgrade.config;
            task::spawn(async move {
                match on_upgrade.await {
                    Ok(upgraded) => {
                        let io = TokioIo::new(upgraded);
                        let stream = WebSocketStream::from_raw_socket(
                            TokioAdapter::new(io),
                            Role::Server,
                            config,
                        )
                        .await;
                        callback(stream).await;
                    }
                    Err(error) => {
                        log::error!("WebSocket upgrade failed: {error}");
                    }
                }
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Body;

    fn build_request() -> Request {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::GET;
        request
    }

    #[tokio::test]
    async fn rejects_invalid_headers() {
        let mut request = build_request();
        assert!(WebSocketUpgrade::extract(&mut request).await.is_err());

        request.headers_mut().insert(
            header::SEC_WEBSOCKET_KEY,
            hyper::header::HeaderValue::from_static("x"),
        );
        request.headers_mut().insert(
            header::CONNECTION,
            hyper::header::HeaderValue::from_static("close"),
        );
        request.headers_mut().insert(
            header::UPGRADE,
            hyper::header::HeaderValue::from_static("websocket"),
        );
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_VERSION,
            hyper::header::HeaderValue::from_static("12"),
        );

        assert!(WebSocketUpgrade::extract(&mut request).await.is_err());
    }

    #[tokio::test]
    async fn accepts_valid_request() {
        let mut request = build_request();
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_KEY,
            hyper::header::HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        request.headers_mut().insert(
            header::CONNECTION,
            hyper::header::HeaderValue::from_static("Upgrade"),
        );
        request.headers_mut().insert(
            header::UPGRADE,
            hyper::header::HeaderValue::from_static("websocket"),
        );
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_VERSION,
            hyper::header::HeaderValue::from_static("13"),
        );
        let on_upgrade = hyper::upgrade::on(&mut request);
        request.extensions_mut().insert(on_upgrade);
        let ws = WebSocketUpgrade::extract(&mut request).await.unwrap();
        assert!(ws.response_protocol.is_none());
    }
}
