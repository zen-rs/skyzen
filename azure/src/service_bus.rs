//! Azure Service Bus implementation of [`MessageQueue`].
//!
//! The transport is the [Service Bus REST API] spoken directly on `reqwest`, rather than a client
//! library: the only published Service Bus crate, `azure_messaging_servicebus` 0.21, is built on
//! legacy `azure_core` 0.21, which logs the outgoing `authorization` header value at debug level
//! (RUSTSEC-2026-0275) and drags `http-types`, `async-std` and `rand 0.7` in behind it. Four
//! requests are all this backend ever issues — send, peek-lock, complete and unlock — so speaking
//! them directly costs less than the client did.
//!
//! The wire format is the one the previous release put on the wire, read out of
//! `azure_messaging_servicebus-0.21.0/src/service_bus/mod.rs`; each place that matters cites it.
//!
//! [Service Bus REST API]: https://learn.microsoft.com/rest/api/servicebus/service-bus-runtime-rest

use core::{fmt, time::Duration};

use base64::Engine as _;
use hmac::{Hmac, Mac as _};
use reqwest::{
    header::{HeaderMap, AUTHORIZATION, RETRY_AFTER},
    Client, Method, Response, StatusCode,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use skyzen_services::queue::{
    BatchSendFailure, MessageQueue, MessageReceipt, QueueError, QueueRetry, ReceiveOptions,
    ReceivedMessage, SendOptions,
};
use time::{OffsetDateTime, UtcOffset};
use url::Url;

use crate::status::{classify, retry_after, AzureStatus};

/// The message property this client tags a base64-encoded body with.
///
/// The same name the SQS backend uses, so a consumer that understands one understands the other.
const CONTENT_ENCODING_PROPERTY: &str = "skyzen-content-encoding";

/// The only value this client ever writes into [`CONTENT_ENCODING_PROPERTY`].
const BASE64_ENCODING: &str = "base64";

/// The host every public-cloud Service Bus namespace lives under.
const HOST_SUFFIX: &str = ".servicebus.windows.net";

/// The environment variable [`ServiceBusQueue::from_env`] reads its connection string from.
const CONNECTION_STRING_ENV: &str = "SERVICEBUS_CONNECTION_STRING";

/// The response header carrying a delivered message's `BrokerProperties`.
///
/// Sent on a request, the same header carries the properties a send sets.
const BROKER_PROPERTIES_HEADER: &str = "brokerproperties";

/// The header Azure services name their own error code in.
const ERROR_CODE_HEADER: &str = "x-ms-error-code";

/// The body of a request that has none, which is what puts `Content-Length: 0` on the wire.
///
/// The REST reference's own peek-lock example sends that header, and
/// `azure_messaging_servicebus-0.21.0/src/service_bus/mod.rs` set it explicitly "to avoid
/// truncation errors" — so a bodyless request here declares its length rather than leaving it to
/// the transport.
const EMPTY_BODY: &str = "";

/// How long the SAS token minted for one request stays valid.
///
/// Every token this backend mints authorizes a single request that is issued immediately, so the
/// life is the same short one for all four: a leaked token is useless within five minutes rather
/// than granting queue access for the hour the legacy client's own tokens lasted.
const TOKEN_TTL: time::Duration = time::Duration::seconds(300);

/// An Azure Service Bus-backed message queue.
///
/// # Wire format
///
/// Payloads that are valid UTF-8 are sent **verbatim**, so JSON produced by `send_json` arrives as
/// plain JSON — the same wire contract as the Cloudflare and in-memory backends. Binary (non-UTF-8)
/// payloads are base64-encoded, because the Service Bus REST API carries a message body as text,
/// and are tagged with a `skyzen-content-encoding: base64` custom message property. The encoding is
/// therefore injective: a body that arrives untagged is the bytes that were sent, tag or no tag, so
/// a literal base64-looking string is never mistaken for an encoded blob.
/// [`receive`](MessageQueue::receive) reverses exactly this, and refuses a tag it did not write
/// rather than guessing.
///
/// # Consuming
///
/// Consumption is peek-lock: [`receive`](MessageQueue::receive) leases messages,
/// [`ack`](MessageQueue::ack) completes them and [`nack`](MessageQueue::nack) abandons them for
/// redelivery. The lease lasts the queue's configured `LockDuration`, which the REST API does not
/// let a single receive override — [`ReceiveOptions::visibility_timeout`] is refused rather than
/// silently ignored.
///
/// # Batch semantics
///
/// The Service Bus REST API has no batch send, so [`send_batch`](MessageQueue::send_batch) sends
/// messages **sequentially**, one request per message. Delivery is not atomic: when some sends
/// fail, the rest are still delivered and [`QueueError::PartialBatch`] names exactly which entries
/// were rejected.
///
/// Cloning is cheap — the HTTP client is a handle onto a shared connection pool.
#[derive(Debug, Clone)]
pub struct ServiceBusQueue {
    /// The HTTP client every request rides on.
    http: Client,
    /// `https://{namespace}.servicebus.windows.net/{queue}`, the root of every request URL.
    queue_url: Url,
    /// The shared-access policy name that signs requests.
    policy_name: String,
    /// The key those signatures are keyed with.
    signing_key: SigningKey,
}

impl ServiceBusQueue {
    /// Create a queue client from a namespace and a shared-access policy.
    ///
    /// `namespace` is the bare namespace name, not a host: `my-namespace`, not
    /// `my-namespace.servicebus.windows.net`.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when the namespace or queue name could not appear in a Service Bus
    /// URL, or when the HTTP client could not be built.
    pub fn new(
        namespace: &str,
        queue: &str,
        policy_name: impl Into<String>,
        signing_key: &str,
    ) -> Result<Self, QueueError> {
        Ok(Self {
            http: Client::builder().build().map_err(|error| {
                QueueError::backend_with("failed to build an HTTPS client for Service Bus", error)
            })?,
            queue_url: queue_url(namespace, queue)?,
            policy_name: policy_name.into(),
            signing_key: SigningKey(signing_key.to_owned()),
        })
    }

    /// Create a queue client from a Service Bus connection string.
    ///
    /// The string is the one the portal calls a "primary connection string":
    /// `Endpoint=sb://…;SharedAccessKeyName=…;SharedAccessKey=…`. When it carries an `EntityPath`,
    /// that entity must be `queue`.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when the connection string is malformed, names a namespace outside
    /// the public cloud, or names a different entity than `queue`;
    /// [`QueueError::Unsupported`] when it authenticates with a pre-minted signature rather than a
    /// key, which this client cannot sign its own requests with.
    pub fn from_connection_string(
        connection_string: &str,
        queue: &str,
    ) -> Result<Self, QueueError> {
        let parsed = ConnectionString::parse(connection_string)?;

        if let Some(entity_path) = &parsed.entity_path {
            if entity_path != queue {
                return Err(QueueError::backend(format!(
                    "the connection string's EntityPath names queue {entity_path:?}, \
                     but {queue:?} was requested"
                )));
            }
        }

        Self::new(
            &parsed.namespace,
            queue,
            parsed.policy_name,
            &parsed.signing_key,
        )
    }

    /// Create a queue client from the `SERVICEBUS_CONNECTION_STRING` environment variable.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when the variable is unset or its value is not a usable connection
    /// string — see [`from_connection_string`](Self::from_connection_string).
    pub fn from_env(queue: &str) -> Result<Self, QueueError> {
        let connection_string = std::env::var(CONNECTION_STRING_ENV).map_err(|error| {
            QueueError::backend_with(
                format!("{CONNECTION_STRING_ENV} is not set to a Service Bus connection string"),
                error,
            )
        })?;

        Self::from_connection_string(&connection_string, queue)
    }

    /// The URL of `segments` under this queue, with every segment encoded by the URL parser.
    fn message_url(&self, segments: &[&str]) -> Result<Url, ServiceBusError> {
        let mut url = self.queue_url.clone();
        url.path_segments_mut()
            .map_err(|()| ServiceBusError::local("the Service Bus queue URL cannot carry a path"))?
            .extend(segments);
        Ok(url)
    }

    /// Mint a shared-access-signature token authorizing one request against `url`.
    ///
    /// The token is the documented Service Bus SAS: an HMAC-SHA256 over the percent-encoded
    /// resource URI and the expiry instant, keyed with the policy's key. It is valid for
    /// [`TOKEN_TTL`].
    fn sas_token(&self, url: &Url) -> Result<String, ServiceBusError> {
        let resource: String =
            url::form_urlencoded::byte_serialize(url.as_str().as_bytes()).collect();
        let expiry = OffsetDateTime::now_utc()
            .checked_add(TOKEN_TTL)
            .ok_or_else(|| ServiceBusError::local("the SAS token expiry overflowed the calendar"))?
            .unix_timestamp();

        let signature: String = url::form_urlencoded::byte_serialize(
            sign(&self.signing_key, &resource, expiry).as_bytes(),
        )
        .collect();

        Ok(format!(
            "SharedAccessSignature sr={resource}&sig={signature}&se={expiry}&skn={}",
            self.policy_name
        ))
    }

    /// Build the request one send goes out as.
    ///
    /// `POST {queue}/messages`, the body carrying the encoded text. A delay becomes the
    /// `BrokerProperties` header and a base64 body its `skyzen-content-encoding` custom property —
    /// the two headers `azure_messaging_servicebus-0.21.0/src/service_bus/mod.rs` put on this
    /// request through `SendMessageOptions`.
    ///
    /// Nothing else, and in particular **no `Content-Type`**: the legacy client sent none, because
    /// this backend never set `SendMessageOptions::content_type`, and a content type declared here
    /// would arrive as the message's own `ContentType` for every consumer — describing a JSON or
    /// text body as whatever this client happened to name. `skyzen-content-encoding` is this
    /// backend's whole encoding contract.
    fn send_request(
        &self,
        encoded: EncodedMessage,
        options: &SendOptions,
    ) -> Result<reqwest::Request, ServiceBusError> {
        let url = self.message_url(&["messages"])?;

        let mut request = self
            .http
            .post(url.clone())
            .header(AUTHORIZATION, self.sas_token(&url)?)
            .body(encoded.body);

        if let Some(delay) = options.delay {
            request = request.header(
                BROKER_PROPERTIES_HEADER,
                SendBrokerProperties::scheduled_at(scheduled_enqueue_time(delay)?).encode()?,
            );
        }

        if encoded.base64 {
            request = request.header(CONTENT_ENCODING_PROPERTY, property_value(BASE64_ENCODING)?);
        }

        request.build().map_err(|error| {
            ServiceBusError::local_with("failed to build a Service Bus send request", error)
        })
    }

    /// Send one message, reporting what the service said about it.
    async fn send_message(
        &self,
        message: &[u8],
        options: SendOptions,
    ) -> Result<(), ServiceBusError> {
        let request = self.send_request(encode_message(message), &options)?;

        let response = self
            .http
            .execute(request)
            .await
            .map_err(|error| ServiceBusError::transport("send", error))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(ServiceBusError::answered("send", response).await)
        }
    }

    /// Lease one message, waiting up to `wait` for one to arrive.
    ///
    /// `POST {queue}/messages/head?timeout=<secs>`, which the service answers with `201` and the
    /// message, or with `204` when the wait elapsed first.
    async fn peek_lock(&self, wait: Duration) -> Result<Option<ReceivedMessage>, QueueError> {
        let mut url = self
            .message_url(&["messages", "head"])
            .map_err(queue_error)?;
        url.query_pairs_mut()
            .append_pair("timeout", &wait.as_secs().to_string());

        let response = self
            .http
            .post(url.clone())
            .header(AUTHORIZATION, self.sas_token(&url).map_err(queue_error)?)
            .body(EMPTY_BODY)
            .send()
            .await
            .map_err(|error| queue_error(ServiceBusError::transport("peek-lock", error)))?;

        let status = response.status();
        let headers = response.headers().clone();

        if !peeked(status, header_text(&headers, RETRY_AFTER.as_str()))? {
            return Ok(None);
        }

        let properties = broker_properties(&headers)?;
        let encoding = content_encoding(&headers)?;
        let body = response.bytes().await.map_err(|error| {
            QueueError::backend_with("failed to read a Service Bus message body", error)
        })?;
        let body = core::str::from_utf8(&body).map_err(|error| {
            QueueError::backend_with("Service Bus delivered a body that is not text", error)
        })?;

        received_message(&properties, encoding.as_deref(), body).map(Some)
    }

    /// Settle a leased message and read the outcome off the status.
    ///
    /// The URL is the one the Service Bus REST API documents for settling a peek-locked message,
    /// `…/{queue}/messages/{messageId|sequenceNumber}/{lockToken}`: `DELETE` completes it, `PUT`
    /// unlocks it.
    ///
    /// The sequence number is what names the message here, which is what the service puts in the
    /// `Location` header of the peek-lock it answered: it is a number the broker assigned, while a
    /// message id is whatever the sender chose to set.
    async fn settle(&self, method: Method, receipt: &MessageReceipt) -> Result<(), QueueError> {
        let receipt = ServiceBusReceipt::decode(receipt)?;
        let url = self
            .message_url(&[
                "messages",
                &receipt.sequence_number.to_string(),
                &receipt.lock_token,
            ])
            .map_err(queue_error)?;

        let response = self
            .http
            .request(method, url.clone())
            .header(AUTHORIZATION, self.sas_token(&url).map_err(queue_error)?)
            .body(EMPTY_BODY)
            .send()
            .await
            .map_err(|error| queue_error(ServiceBusError::transport("settle", error)))?;

        let status = response.status();
        settled(
            status,
            header_text(response.headers(), RETRY_AFTER.as_str()),
        )
    }
}

/// The shared-access key requests are signed with.
///
/// A newtype rather than a bare `String` so that no `Debug` of anything holding it can print it:
/// a client that logged its own credentials is what this backend exists to stop shipping.
#[derive(Clone)]
struct SigningKey(String);

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

/// The `sig` of a SAS token: the base64 HMAC-SHA256 of `"{resource}\n{expiry}"`.
///
/// The key is the policy key's own characters taken as bytes. The legacy client reached the same
/// bytes the long way around — `QueueClient::new` base64-*encoded* the key
/// (`azure_messaging_servicebus-0.21.0/src/service_bus/queue_client.rs`) purely so that
/// `azure_core`'s `hmac_sha256` could base64-*decode* it again
/// (`azure_core-0.21.0/src/hmac.rs`) — so the signature is byte-identical.
fn sign(key: &SigningKey, resource: &str, expiry: i64) -> String {
    let mut hmac = Hmac::<Sha256>::new_from_slice(key.0.as_bytes())
        .expect("HMAC-SHA256 accepts a key of any length");
    hmac.update(format!("{resource}\n{expiry}").as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hmac.finalize().into_bytes())
}

/// The URL of a queue in a namespace, with every segment encoded by the URL parser.
fn queue_url(namespace: &str, queue: &str) -> Result<Url, QueueError> {
    if namespace.is_empty() || !namespace.chars().all(is_namespace_char) {
        return Err(QueueError::backend(format!(
            "{namespace:?} is not a Service Bus namespace name; it must be the bare name \
             (letters, digits and hyphens), not a host or a URL"
        )));
    }

    if queue.is_empty() || !queue.chars().all(is_entity_char) {
        return Err(QueueError::backend(format!(
            "{queue:?} is not a Service Bus entity name; entity names hold letters, digits, \
             periods, hyphens, underscores and slashes"
        )));
    }

    let mut url = Url::parse(&format!("https://{namespace}{HOST_SUFFIX}")).map_err(|error| {
        QueueError::backend_with(
            format!("namespace {namespace:?} is not a valid host"),
            error,
        )
    })?;
    url.path_segments_mut()
        .map_err(|()| QueueError::backend("the Service Bus namespace URL cannot carry a path"))?
        .extend(queue.split('/'));
    Ok(url)
}

/// Whether `c` may appear in the DNS label of a Service Bus namespace.
const fn is_namespace_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// Whether `c` may appear in a Service Bus entity name.
///
/// Service Bus entity names are letters, digits, periods, hyphens, underscores and slashes (which
/// separate the segments of a hierarchical name). Nothing here needs percent-encoding, which is
/// what makes an entity name safe to sign as part of a resource URI.
const fn is_entity_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/')
}

/// A parsed Service Bus connection string.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectionString {
    /// The bare namespace name taken from `Endpoint`.
    namespace: String,
    /// `SharedAccessKeyName`.
    policy_name: String,
    /// `SharedAccessKey`.
    signing_key: String,
    /// `EntityPath`, when the string names one entity rather than the whole namespace.
    entity_path: Option<String>,
}

impl ConnectionString {
    /// Parse `Endpoint=sb://…;SharedAccessKeyName=…;SharedAccessKey=…[;EntityPath=…]`.
    fn parse(connection_string: &str) -> Result<Self, QueueError> {
        let mut endpoint = None;
        let mut policy_name = None;
        let mut signing_key = None;
        let mut entity_path = None;

        for part in connection_string.split(';').filter(|part| !part.is_empty()) {
            // A key's value can itself contain `=` — a base64 key almost always ends in one — so
            // only the first separator splits.
            let (name, value) = part.split_once('=').ok_or_else(|| {
                QueueError::backend(format!(
                    "the Service Bus connection string holds {part:?}, which is not a name=value pair"
                ))
            })?;

            let name = name.trim();
            let value = value.trim().to_owned();

            if name.eq_ignore_ascii_case("Endpoint") {
                endpoint = Some(value);
            } else if name.eq_ignore_ascii_case("SharedAccessKeyName") {
                policy_name = Some(value);
            } else if name.eq_ignore_ascii_case("SharedAccessKey") {
                signing_key = Some(value);
            } else if name.eq_ignore_ascii_case("EntityPath") {
                entity_path = Some(value);
            } else if name.eq_ignore_ascii_case("SharedAccessSignature") {
                return Err(QueueError::Unsupported(
                    "a connection string that carries a pre-minted SharedAccessSignature cannot \
                     sign the requests this backend issues; use a connection string with a \
                     SharedAccessKey",
                ));
            }
        }

        let endpoint = endpoint.ok_or_else(|| {
            QueueError::backend("the Service Bus connection string has no Endpoint")
        })?;

        Ok(Self {
            namespace: namespace_from_endpoint(&endpoint)?,
            policy_name: policy_name.ok_or_else(|| {
                QueueError::backend("the Service Bus connection string has no SharedAccessKeyName")
            })?,
            signing_key: signing_key.ok_or_else(|| {
                QueueError::backend("the Service Bus connection string has no SharedAccessKey")
            })?,
            entity_path,
        })
    }
}

/// The bare namespace name inside `sb://my-namespace.servicebus.windows.net/`.
///
/// A host outside `.servicebus.windows.net` is refused rather than accepted and then ignored: every
/// request URL is built against the public-cloud host, so a sovereign-cloud endpoint would silently
/// address the wrong namespace.
fn namespace_from_endpoint(endpoint: &str) -> Result<String, QueueError> {
    let url = Url::parse(endpoint).map_err(|error| {
        QueueError::backend_with(
            format!("the connection string's Endpoint {endpoint:?} is not a URL"),
            error,
        )
    })?;

    let host = url.host_str().ok_or_else(|| {
        QueueError::backend(format!(
            "the connection string's Endpoint {endpoint:?} names no host"
        ))
    })?;

    host.strip_suffix(HOST_SUFFIX)
        .filter(|namespace| !namespace.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            QueueError::backend(format!(
                "the connection string's Endpoint {endpoint:?} is outside {HOST_SUFFIX}; \
                 the Service Bus REST API this backend speaks addresses the public cloud only"
            ))
        })
}

/// A message body encoded for Service Bus transport.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EncodedMessage {
    /// The text to send as the message body.
    body: String,
    /// Whether the body is base64-encoded, and so must carry the encoding property.
    base64: bool,
}

/// Encode a payload: verbatim when it is valid UTF-8, base64 otherwise.
fn encode_message(message: &[u8]) -> EncodedMessage {
    core::str::from_utf8(message).map_or_else(
        |_| EncodedMessage {
            body: base64::engine::general_purpose::STANDARD.encode(message),
            base64: true,
        },
        |text| EncodedMessage {
            body: text.to_owned(),
            base64: false,
        },
    )
}

/// Reverse [`encode_message`] for one delivered body.
///
/// An untagged body is the bytes Service Bus delivered. A body tagged with an encoding this client
/// never writes is refused rather than handed back wrongly decoded.
fn decode_message(body: &str, encoding: Option<&str>) -> Result<Vec<u8>, QueueError> {
    match encoding {
        None => Ok(body.as_bytes().to_vec()),
        Some(BASE64_ENCODING) => base64::engine::general_purpose::STANDARD
            .decode(body)
            .map_err(|error| {
                QueueError::backend_with(
                    format!(
                        "message body tagged `{CONTENT_ENCODING_PROPERTY}: {BASE64_ENCODING}` \
                         is not valid base64"
                    ),
                    error,
                )
            }),
        Some(other) => Err(QueueError::backend(format!(
            "message carries an unknown `{CONTENT_ENCODING_PROPERTY}` of {other:?}; \
             this client only writes {BASE64_ENCODING:?}"
        ))),
    }
}

/// One header's value as text, when the response carries it and it is text at all.
fn header_text<'headers>(headers: &'headers HeaderMap, name: &str) -> Option<&'headers str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// A custom message property's value, as Service Bus encodes one.
///
/// The REST API carries a custom property as its own header holding a JSON value, and hands it back
/// the same way: the `Peek-Lock Message` reference answers a message whose properties were sent as
/// `Priority: High` with `Priority: "High"`. So a string property goes out quoted and comes back
/// quoted, which is what [`content_encoding`] reads.
fn property_value(value: &str) -> Result<String, ServiceBusError> {
    serde_json::to_string(value).map_err(|error| {
        ServiceBusError::local_with(
            format!("failed to encode the {CONTENT_ENCODING_PROPERTY} message property"),
            error,
        )
    })
}

/// The `BrokerProperties` of a delivered message, which is what makes it settleable.
fn broker_properties(headers: &HeaderMap) -> Result<BrokerProperties, QueueError> {
    let header = header_text(headers, BROKER_PROPERTIES_HEADER).ok_or_else(|| {
        QueueError::backend(
            "Service Bus delivered a message with no BrokerProperties, which can never be settled",
        )
    })?;

    serde_json::from_str(header).map_err(|error| {
        QueueError::backend_with(
            "Service Bus delivered BrokerProperties this backend could not read",
            error,
        )
    })
}

/// The `skyzen-content-encoding` property of a delivered message, when it carries one.
fn content_encoding(headers: &HeaderMap) -> Result<Option<String>, QueueError> {
    header_text(headers, CONTENT_ENCODING_PROPERTY)
        .map(|value| {
            serde_json::from_str(value).map_err(|error| {
                QueueError::backend_with(
                    format!(
                        "the {CONTENT_ENCODING_PROPERTY} property of a delivered message is \
                         {value:?}, which is not the JSON string Service Bus encodes a property as"
                    ),
                    error,
                )
            })
        })
        .transpose()
}

/// The lease this backend hands back, and takes back to settle a message.
///
/// Service Bus settles a peek-locked message by naming both the message and its lock token in the
/// request URL, so a receipt has to carry both.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ServiceBusReceipt {
    /// The `SequenceNumber` broker property, which is how the service's own `Location` header
    /// names the locked message.
    sequence_number: i64,
    /// The `LockToken` broker property, which names this delivery's lock.
    lock_token: String,
}

impl ServiceBusReceipt {
    /// Render the receipt as the opaque token [`MessageReceipt`] carries.
    fn encode(&self) -> Result<MessageReceipt, QueueError> {
        Ok(MessageReceipt::new(serde_json::to_string(self)?))
    }

    /// Read back a receipt this backend minted.
    fn decode(receipt: &MessageReceipt) -> Result<Self, QueueError> {
        serde_json::from_str(receipt.as_str()).map_err(|error| {
            QueueError::backend_with(
                "the receipt handed to `ack`/`nack` was not minted by the Service Bus backend",
                error,
            )
        })
    }
}

/// The broker properties of a delivered message, as far as this backend reads them.
///
/// Deliberately **not** `deny_unknown_fields`: the broker sends `State`, `TimeToLive`,
/// `EnqueuedTimeUtc` and whatever else the message carried, and a property this backend has no use
/// for must not fail a delivery.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BrokerProperties {
    /// How many times this message has been delivered, this delivery included.
    delivery_count: i64,
    /// The lock this delivery holds, which a settlement names.
    lock_token: String,
    /// The message id the sender set.
    message_id: String,
    /// The number the broker assigned this message, which a settlement names.
    sequence_number: i64,
}

/// The broker properties a send sets.
///
/// One property, and the wire form is the one
/// `azure_messaging_servicebus-0.21.0/src/service_bus/mod.rs` produced: its
/// `SettableBrokerProperties` is `PascalCase` with `skip_serializing_if = "Option::is_none"`, so a
/// send that scheduled nothing else put exactly `{"ScheduledEnqueueTimeUtc":…}` on the wire, and
/// that field is written with `time::serde::rfc2822` — `Wed, 02 Jul 2014 01:32:27 +0000`, not RFC
/// 3339 and not the `GMT` spelling the service's own responses use.
#[derive(Debug, Serialize)]
struct SendBrokerProperties {
    /// The instant the message becomes visible in the queue.
    #[serde(rename = "ScheduledEnqueueTimeUtc", with = "time::serde::rfc2822")]
    scheduled_enqueue_time_utc: OffsetDateTime,
}

impl SendBrokerProperties {
    /// The properties scheduling a message for `instant`.
    const fn scheduled_at(instant: OffsetDateTime) -> Self {
        Self {
            scheduled_enqueue_time_utc: instant,
        }
    }

    /// The `BrokerProperties` header value these properties travel as.
    fn encode(&self) -> Result<String, ServiceBusError> {
        serde_json::to_string(self).map_err(|error| {
            ServiceBusError::local_with("failed to encode a scheduled delivery time", error)
        })
    }
}

/// The instant `delay` from now, for Service Bus' `ScheduledEnqueueTimeUtc` broker property.
///
/// The offset is pinned to UTC: the property names a UTC instant, and RFC 2822 renders whatever
/// offset the value carries.
fn scheduled_enqueue_time(delay: Duration) -> Result<OffsetDateTime, ServiceBusError> {
    let delay = time::Duration::try_from(delay).map_err(|error| {
        ServiceBusError::local_with(
            "the requested delivery delay does not fit a Service Bus schedule",
            error,
        )
    })?;

    OffsetDateTime::now_utc()
        .checked_add(delay)
        .map(|instant| instant.to_offset(UtcOffset::UTC))
        .ok_or_else(|| {
            ServiceBusError::local("the requested delivery delay overflowed the calendar")
        })
}

/// Turn one leased delivery into a message.
fn received_message(
    properties: &BrokerProperties,
    encoding: Option<&str>,
    body: &str,
) -> Result<ReceivedMessage, QueueError> {
    let receipt = ServiceBusReceipt {
        sequence_number: properties.sequence_number,
        lock_token: properties.lock_token.clone(),
    };

    Ok(ReceivedMessage {
        id: Some(properties.message_id.clone()),
        body: decode_message(body, encoding)?,
        receipt: receipt.encode()?,
        attempts: Some(u32::try_from(properties.delivery_count).map_err(|error| {
            QueueError::backend_with(
                format!(
                    "Service Bus reported a DeliveryCount of {}, which is not a count",
                    properties.delivery_count
                ),
                error,
            )
        })?),
    })
}

/// Whether a peek-lock answered with a message, and refuse anything but a message or an empty queue.
fn peeked(status: StatusCode, retry_after_header: Option<&str>) -> Result<bool, QueueError> {
    match status {
        StatusCode::OK | StatusCode::CREATED => Ok(true),
        // Service Bus answers a peek-lock that waited out its timeout with an empty 204.
        StatusCode::NO_CONTENT => Ok(false),
        status => Err(status_error(status, "peek-lock", retry_after_header)),
    }
}

/// Read the outcome of a settlement request off its status.
///
/// Service Bus answers a settle it cannot match with `404` and a settle against a queue that does
/// not exist with `410`, so only the first is the message state changing underneath the call.
fn settled(status: StatusCode, retry_after_header: Option<&str>) -> Result<(), QueueError> {
    if status.is_success() {
        return Ok(());
    }

    match classify(status.as_u16()) {
        // The lock this receipt named is gone: it lapsed and the message went back to the queue, or
        // it was settled already. That is the message state changing underneath the call, not a
        // backend failure.
        AzureStatus::Absent | AzureStatus::Conflict => Err(QueueError::Conflict),
        _ => Err(status_error(status, "settle", retry_after_header)),
    }
}

/// Map a failing Service Bus status onto the portable taxonomy.
fn status_error(
    status: StatusCode,
    operation: &str,
    retry_after_header: Option<&str>,
) -> QueueError {
    match classify(status.as_u16()) {
        AzureStatus::Throttled => QueueError::Throttled {
            retry_after: retry_after_header.and_then(retry_after),
        },
        AzureStatus::Unauthorized => QueueError::Unauthorized,
        AzureStatus::Conflict => QueueError::Conflict,
        _ => QueueError::backend(format!(
            "Service Bus answered a {operation} request with {}",
            status.as_u16()
        )),
    }
}

/// A Service Bus request that failed.
///
/// It carries what the service said — the status, its own error code when it sent one, and the
/// `Retry-After` of a throttled answer — because [`queue_error`] and [`batch_failure_code`] read
/// exactly that. It never carries the `Authorization` header or the signing key, which is the
/// whole point of not logging a request.
#[derive(Debug)]
struct ServiceBusError {
    /// What the service answered, when the request reached it at all.
    answer: Option<ServiceAnswer>,
    /// What went wrong, in words.
    message: String,
    /// The failure underneath, when there was one.
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

/// The part of a failing answer this backend reads.
#[derive(Debug)]
struct ServiceAnswer {
    /// The status the service answered with.
    status: StatusCode,
    /// The service's own error code, from `x-ms-error-code`.
    code: Option<String>,
    /// The delay a throttled answer asked for.
    retry_after: Option<Duration>,
}

impl ServiceBusError {
    /// A failure of this client's own, before or after the wire.
    fn local(message: impl Into<String>) -> Self {
        Self {
            answer: None,
            message: message.into(),
            source: None,
        }
    }

    /// A failure of this client's own, keeping the error underneath it.
    fn local_with(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            answer: None,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// A request that never got an answer: a refused connection, a TLS failure, a timeout.
    fn transport(operation: &str, source: reqwest::Error) -> Self {
        Self {
            answer: None,
            message: format!("the {operation} request to Service Bus failed"),
            source: Some(Box::new(source)),
        }
    }

    /// A request the service refused, read out of its answer.
    async fn answered(operation: &str, response: Response) -> Self {
        let status = response.status();
        let code = header_text(response.headers(), ERROR_CODE_HEADER).map(ToOwned::to_owned);
        let retry_after =
            header_text(response.headers(), RETRY_AFTER.as_str()).and_then(retry_after);

        // Service Bus explains a refusal in the body — an XML `<Error><Code>…</Code>` document —
        // and a body this backend cannot read is no reason to lose the status.
        let body = response.bytes().await.map_or_else(
            |_| String::new(),
            |body| String::from_utf8_lossy(&body).trim().to_owned(),
        );

        Self {
            answer: Some(ServiceAnswer {
                status,
                code,
                retry_after,
            }),
            message: if body.is_empty() {
                format!("Service Bus answered a {operation} request with {status}")
            } else {
                format!("Service Bus answered a {operation} request with {status}: {body}")
            },
            source: None,
        }
    }
}

impl fmt::Display for ServiceBusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ServiceBusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| &**source as &(dyn std::error::Error + 'static))
    }
}

/// Map a failed request onto the portable taxonomy, keeping its source chain.
fn queue_error(error: ServiceBusError) -> QueueError {
    match error
        .answer
        .as_ref()
        .map(|answer| (classify(answer.status.as_u16()), answer.retry_after))
    {
        Some((AzureStatus::Throttled, retry_after)) => QueueError::Throttled { retry_after },
        Some((AzureStatus::Unauthorized, _)) => QueueError::Unauthorized,
        Some((AzureStatus::Conflict, _)) => QueueError::Conflict,
        _ => backend_error(error),
    }
}

/// The service's own code for a rejected batch entry.
///
/// Service Bus has no batch send, so there is no per-entry code to quote: the closest thing the
/// service says about one rejected message is the error code (when it sends one) or the status of
/// that message's own request. A request that never reached the service has neither.
fn batch_failure_code(error: &ServiceBusError) -> String {
    error.answer.as_ref().map_or_else(
        || "Transport".to_owned(),
        |answer| {
            answer
                .code
                .clone()
                .unwrap_or_else(|| answer.status.as_u16().to_string())
        },
    )
}

/// Map any error with a source chain onto [`QueueError::Backend`].
fn backend_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> QueueError {
    QueueError::backend_with(error.to_string(), error)
}

impl MessageQueue for ServiceBusQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        self.send_message(message, SendOptions::new())
            .await
            .map_err(queue_error)
    }

    /// Send one message, scheduling it when [`SendOptions::delay`] asks for one.
    ///
    /// A delay becomes Service Bus' `ScheduledEnqueueTimeUtc` broker property, so the message sits
    /// in the queue invisible until that instant.
    async fn send_with(&self, message: &[u8], options: SendOptions) -> Result<(), QueueError> {
        self.send_message(message, options)
            .await
            .map_err(queue_error)
    }

    /// Send messages sequentially: the Service Bus REST API has no batch send.
    ///
    /// Not atomic. Every message the batch carried whose index is absent from
    /// [`QueueError::PartialBatch`] was delivered, and re-sending only the named failures is what
    /// recovers the batch.
    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        let mut failures = Vec::new();

        for (index, message) in messages.iter().enumerate() {
            if let Err(error) = self.send_message(message, SendOptions::new()).await {
                failures.push(BatchSendFailure {
                    index,
                    code: batch_failure_code(&error),
                    message: error.to_string(),
                });
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(QueueError::PartialBatch { failures })
        }
    }

    /// Take up to [`ReceiveOptions::max_messages`] messages, leasing each one.
    ///
    /// Service Bus locks one message per peek-lock request, so a batch of `n` is `n` requests that
    /// stop at the first empty answer. Only that last request waits — every earlier one is answered
    /// at once by a message that is already there — so a batch costs one [`ReceiveOptions::wait`]
    /// in total rather than one per message.
    ///
    /// # Platform notes
    ///
    /// - [`ReceiveOptions::wait`] is **required**. A peek-lock always waits for a message: the REST
    ///   API documents no non-blocking form, so there is nothing to map `None` — "answer with
    ///   whatever is available immediately" — onto. Rather than block for the service's own
    ///   default and call it immediate, a `None` wait is refused, naming the wait to pass instead.
    /// - [`ReceiveOptions::visibility_timeout`] is refused: the lease lasts the queue's configured
    ///   `LockDuration`, and the REST API has no way to override it for one delivery.
    /// - A request that fails part-way through a batch fails the whole call. The messages already
    ///   locked by it are never handed back, so they return to the queue when their locks lapse —
    ///   at-least-once delivery, as peek-lock consumption is everywhere.
    async fn receive(&self, options: ReceiveOptions) -> Result<Vec<ReceivedMessage>, QueueError> {
        if options.max_messages == 0 {
            return Err(QueueError::backend(
                "ReceiveOptions::max_messages must be at least 1; asking Service Bus for zero \
                 messages would look the same as an empty queue",
            ));
        }

        if options.visibility_timeout.is_some() {
            return Err(QueueError::Unsupported(
                "Service Bus leases a peek-locked message for the queue's configured LockDuration; \
                 its REST API cannot set a per-delivery visibility timeout",
            ));
        }

        let Some(wait) = options.wait else {
            return Err(QueueError::Unsupported(
                "Service Bus peek-lock has no documented non-blocking receive form, so this \
                 backend cannot answer a `wait` of `None` immediately; pass an explicit wait, \
                 e.g. `ReceiveOptions::new().with_wait(Duration::from_secs(30))`",
            ));
        };

        let mut messages = Vec::with_capacity(options.max_messages);
        for _ in 0..options.max_messages {
            let Some(message) = self.peek_lock(wait).await? else {
                break;
            };

            messages.push(message);
        }

        Ok(messages)
    }

    /// Complete a leased message, which deletes it from the queue.
    async fn ack(&self, receipt: &MessageReceipt) -> Result<(), QueueError> {
        self.settle(Method::DELETE, receipt).await
    }

    /// Abandon a leased message, which returns it to the queue for immediate redelivery.
    ///
    /// [`QueueRetry::delay_seconds`] above zero is refused: abandoning a Service Bus message
    /// releases its lock at once, and the API has no way to hold the redelivery back.
    async fn nack(&self, receipt: &MessageReceipt, retry: QueueRetry) -> Result<(), QueueError> {
        if retry.delay_seconds.is_some_and(|delay| delay > 0) {
            return Err(QueueError::Unsupported(
                "Service Bus redelivers an abandoned message as soon as its lock is released; \
                 delaying a retry needs a scheduled re-send instead",
            ));
        }

        self.settle(Method::PUT, receipt).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        batch_failure_code, content_encoding, decode_message, encode_message, peeked,
        property_value, queue_url, received_message, settled, sign, BrokerProperties,
        ConnectionString, SendBrokerProperties, ServiceAnswer, ServiceBusError, ServiceBusQueue,
        ServiceBusReceipt, SigningKey, BROKER_PROPERTIES_HEADER, CONTENT_ENCODING_PROPERTY,
    };
    use base64::Engine as _;
    use core::time::Duration;
    use reqwest::{
        header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE},
        StatusCode,
    };
    use skyzen_services::queue::{
        MessageQueue, MessageReceipt, QueueError, QueueRetry, ReceiveOptions, SendOptions,
    };
    use time::OffsetDateTime;

    const CONNECTION_STRING: &str = "Endpoint=sb://skyzen-test.servicebus.windows.net/;\
         SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2t5emVuLXRlc3Qta2V5";

    /// The `BrokerProperties` header of the `Peek-Lock Message` reference's own example answer.
    const EXAMPLE_BROKER_PROPERTIES: &str = concat!(
        r#"{"DeliveryCount":1,"EnqueuedSequenceNumber":0,"#,
        r#""EnqueuedTimeUtc":"Wed, 02 Jul 2014 01:32:27 GMT","Label":"M1","#,
        r#""LockToken":"7da9cfd5-40d5-4bb1-8d64-ec5a52e1c547","#,
        r#""LockedUntilUtc":"Wed, 02 Jul 2014 01:33:27 GMT","#,
        r#""MessageId":"31907572164743c38741631acd554d6f","SequenceNumber":2,"#,
        r#""State":"Active","TimeToLive":10}"#,
    );

    fn queue() -> ServiceBusQueue {
        ServiceBusQueue::from_connection_string(CONNECTION_STRING, "jobs").expect("should build")
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).expect("a header value"));
        }
        headers
    }

    #[test]
    fn json_passes_through_unchanged() {
        let payload = br#"{"kind":"email"}"#;
        let encoded = encode_message(payload);
        assert_eq!(encoded.body.as_bytes(), payload);
        assert!(!encoded.base64);
    }

    #[test]
    fn unicode_text_passes_through_unchanged() {
        let payload = "hello 世界".as_bytes();
        let encoded = encode_message(payload);
        assert_eq!(encoded.body.as_bytes(), payload);
        assert!(!encoded.base64);
    }

    #[test]
    fn binary_payload_is_base64_encoded_and_tagged() {
        let payload = [0xFF, 0xFE, 0x00, 0x01];
        let encoded = encode_message(&payload);
        assert!(encoded.base64);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&encoded.body)
                .unwrap(),
            payload
        );
    }

    #[test]
    fn the_encoding_is_injective_for_a_body_that_looks_base64() {
        // The exact ambiguity the tag exists to remove: this text *is* valid base64, and a
        // receiver must still get the text back rather than the bytes it would decode to.
        let payload = b"aGVsbG8=";
        let encoded = encode_message(payload);
        assert!(!encoded.base64);
        assert_eq!(
            decode_message(&encoded.body, None).unwrap(),
            payload.to_vec()
        );
    }

    #[test]
    fn a_tagged_body_round_trips_through_the_decoder() {
        let payload = [0x00, 0x9F, 0x92, 0x96];
        let encoded = encode_message(&payload);
        assert_eq!(
            decode_message(&encoded.body, Some("base64")).unwrap(),
            payload.to_vec()
        );
    }

    #[test]
    fn an_unknown_encoding_tag_is_refused_rather_than_guessed() {
        let error = decode_message("body", Some("gzip")).unwrap_err();
        assert!(error.to_string().contains("unknown"));
    }

    #[test]
    fn a_body_tagged_base64_that_is_not_base64_is_refused() {
        assert!(decode_message("not base64!", Some("base64")).is_err());
    }

    #[test]
    fn a_custom_property_travels_as_a_json_string_in_both_directions() {
        // What the REST API documents: a property is a JSON value in its own header, and the
        // service hands a string property back quoted.
        let value = property_value("base64").expect("should encode");
        assert_eq!(value, "\"base64\"");

        let read = content_encoding(&headers(&[(CONTENT_ENCODING_PROPERTY, &value)]))
            .expect("should read");
        assert_eq!(read.as_deref(), Some("base64"));

        assert!(content_encoding(&HeaderMap::new())
            .expect("no property is not a failure")
            .is_none());
    }

    #[test]
    fn a_custom_property_that_is_not_a_json_string_is_refused() {
        let error = content_encoding(&headers(&[(CONTENT_ENCODING_PROPERTY, "base64")]))
            .expect_err("an unquoted property should be refused");
        assert!(error.to_string().contains(CONTENT_ENCODING_PROPERTY));
    }

    #[test]
    fn a_scheduled_send_writes_the_broker_property_the_service_reads() {
        let instant = OffsetDateTime::from_unix_timestamp(1_404_264_747).expect("a valid instant");
        let properties = SendBrokerProperties::scheduled_at(instant).encode();

        // RFC 2822, which is what the legacy client's `time::serde::rfc2822` wrote.
        assert_eq!(
            properties.expect("should encode"),
            r#"{"ScheduledEnqueueTimeUtc":"Wed, 02 Jul 2014 01:32:27 +0000"}"#
        );
    }

    #[test]
    fn a_send_names_the_queue_and_declares_no_content_type() {
        let queue = queue();
        let request = queue
            .send_request(encode_message(br#"{"kind":"email"}"#), &SendOptions::new())
            .expect("should build a send request");

        assert_eq!(
            request.url().as_str(),
            "https://skyzen-test.servicebus.windows.net/jobs/messages"
        );
        assert!(request.headers().contains_key(AUTHORIZATION));
        // A content type declared here would arrive as the message's own `ContentType`, describing
        // a JSON body as whatever this client named. The previous release sent none either.
        assert!(
            request.headers().get(CONTENT_TYPE).is_none(),
            "{:?}",
            request.headers()
        );
        // Text is not base64, so nothing tags it.
        assert!(request.headers().get(CONTENT_ENCODING_PROPERTY).is_none());
        assert!(request.headers().get(BROKER_PROPERTIES_HEADER).is_none());
    }

    #[test]
    fn a_binary_or_delayed_send_carries_its_property_and_nothing_more() {
        let queue = queue();

        let binary = queue
            .send_request(encode_message(&[0xFF, 0xFE]), &SendOptions::new())
            .expect("should build a send request");
        assert_eq!(
            binary.headers().get(CONTENT_ENCODING_PROPERTY).unwrap(),
            "\"base64\""
        );
        assert!(binary.headers().get(CONTENT_TYPE).is_none());

        let delayed = queue
            .send_request(
                encode_message(b"later"),
                &SendOptions::new().with_delay(Duration::from_secs(60)),
            )
            .expect("should build a send request");
        let scheduled = delayed
            .headers()
            .get(BROKER_PROPERTIES_HEADER)
            .expect("a delayed send schedules the message")
            .to_str()
            .expect("the header is text");
        assert!(
            scheduled.starts_with(r#"{"ScheduledEnqueueTimeUtc":"#),
            "{scheduled}"
        );
        assert!(delayed.headers().get(CONTENT_TYPE).is_none());
    }

    #[test]
    fn the_brokers_own_properties_are_read_and_the_rest_ignored() {
        let properties: BrokerProperties =
            serde_json::from_str(EXAMPLE_BROKER_PROPERTIES).expect("should parse");

        assert_eq!(properties.sequence_number, 2);
        assert_eq!(
            properties.lock_token,
            "7da9cfd5-40d5-4bb1-8d64-ec5a52e1c547"
        );
        assert_eq!(properties.message_id, "31907572164743c38741631acd554d6f");
        assert_eq!(properties.delivery_count, 1);
    }

    #[test]
    fn a_delivery_becomes_a_message_carrying_its_lease() {
        let properties: BrokerProperties =
            serde_json::from_str(EXAMPLE_BROKER_PROPERTIES).expect("should parse");
        let message =
            received_message(&properties, None, "This is a message.").expect("should convert");

        assert_eq!(
            message.id.as_deref(),
            Some("31907572164743c38741631acd554d6f")
        );
        assert_eq!(message.body, b"This is a message.".to_vec());
        assert_eq!(message.attempts, Some(1));
        assert_eq!(
            ServiceBusReceipt::decode(&message.receipt).unwrap(),
            ServiceBusReceipt {
                sequence_number: 2,
                lock_token: "7da9cfd5-40d5-4bb1-8d64-ec5a52e1c547".to_owned(),
            }
        );
    }

    #[test]
    fn a_delivery_with_no_broker_properties_can_never_be_settled() {
        let error = super::broker_properties(&HeaderMap::new())
            .expect_err("a message with no lock is not settleable");
        assert!(error.to_string().contains("BrokerProperties"));
    }

    #[test]
    fn receipts_round_trip_through_their_opaque_token() {
        let receipt = ServiceBusReceipt {
            sequence_number: 42,
            lock_token: "0f6a9d16-9a3d-4c53-9f3a-1c31f2b0f9e2".to_owned(),
        };

        let encoded = receipt.encode().expect("receipt should encode");
        assert_eq!(ServiceBusReceipt::decode(&encoded).unwrap(), receipt);
    }

    #[test]
    fn a_receipt_from_another_backend_is_refused() {
        let error = ServiceBusReceipt::decode(&MessageReceipt::new("sqs-receipt-handle"))
            .expect_err("a foreign receipt should not decode");
        assert!(error.to_string().contains("not minted"));
    }

    #[test]
    fn a_connection_string_yields_the_namespace_policy_and_key() {
        let parsed = ConnectionString::parse(CONNECTION_STRING).expect("should parse");
        assert_eq!(parsed.namespace, "skyzen-test");
        assert_eq!(parsed.policy_name, "RootManageSharedAccessKey");
        assert_eq!(parsed.signing_key, "c2t5emVuLXRlc3Qta2V5");
        assert_eq!(parsed.entity_path, None);
    }

    #[test]
    fn a_key_containing_padding_keeps_every_character() {
        let parsed = ConnectionString::parse(
            "Endpoint=sb://skyzen-test.servicebus.windows.net/;SharedAccessKeyName=policy;\
             SharedAccessKey=YWJjZGVmZ2hpamts==",
        )
        .expect("should parse");
        assert_eq!(parsed.signing_key, "YWJjZGVmZ2hpamts==");
    }

    #[test]
    fn an_entity_path_is_read_and_must_match_the_queue() {
        let parsed = ConnectionString::parse(
            "Endpoint=sb://skyzen-test.servicebus.windows.net/;SharedAccessKeyName=policy;\
             SharedAccessKey=a2V5;EntityPath=jobs",
        )
        .expect("should parse");
        assert_eq!(parsed.entity_path.as_deref(), Some("jobs"));

        let error = ServiceBusQueue::from_connection_string(
            "Endpoint=sb://skyzen-test.servicebus.windows.net/;SharedAccessKeyName=policy;\
             SharedAccessKey=a2V5;EntityPath=jobs",
            "other",
        )
        .expect_err("a mismatched entity path should be refused");
        assert!(error.to_string().contains("EntityPath"));
    }

    #[test]
    fn a_malformed_connection_string_is_refused() {
        assert!(ConnectionString::parse("").is_err());
        assert!(ConnectionString::parse("Endpoint").is_err());
        assert!(ConnectionString::parse(
            "Endpoint=sb://skyzen-test.servicebus.windows.net/;SharedAccessKey=a2V5"
        )
        .is_err());
        assert!(ConnectionString::parse(
            "Endpoint=sb://skyzen-test.servicebus.windows.net/;SharedAccessKeyName=policy"
        )
        .is_err());
    }

    #[test]
    fn an_endpoint_outside_the_public_cloud_is_refused() {
        let error = ConnectionString::parse(
            "Endpoint=sb://skyzen-test.servicebus.chinacloudapi.cn/;SharedAccessKeyName=policy;\
             SharedAccessKey=a2V5",
        )
        .expect_err("a sovereign-cloud endpoint should be refused");
        assert!(error.to_string().contains("servicebus.windows.net"));
    }

    #[test]
    fn a_pre_minted_signature_is_refused_because_requests_cannot_be_signed() {
        let error = ConnectionString::parse(
            "Endpoint=sb://skyzen-test.servicebus.windows.net/;\
             SharedAccessSignature=SharedAccessSignature sr=x&sig=y&se=1&skn=z",
        )
        .expect_err("a signature-only connection string should be refused");
        assert!(matches!(error, QueueError::Unsupported(_)));
    }

    #[test]
    fn the_queue_url_names_the_namespace_host_and_the_entity_path() {
        let url = queue_url("skyzen-test", "jobs/priority").expect("should build");
        assert_eq!(
            url.as_str(),
            "https://skyzen-test.servicebus.windows.net/jobs/priority"
        );
    }

    #[test]
    fn a_namespace_or_queue_that_could_not_appear_in_a_url_is_refused() {
        assert!(queue_url("skyzen-test.servicebus.windows.net", "jobs").is_err());
        assert!(queue_url("", "jobs").is_err());
        assert!(queue_url("skyzen-test", "jobs?evil=1").is_err());
        assert!(queue_url("skyzen-test", "").is_err());
    }

    #[test]
    fn the_settlement_url_names_the_message_and_its_lock() {
        let url = queue()
            .message_url(&["messages", "2", "7da9cfd5-40d5-4bb1-8d64-ec5a52e1c547"])
            .expect("should build a settle URL");

        // The shape the service itself hands back in the `Location` header of a peek-lock.
        assert_eq!(
            url.as_str(),
            "https://skyzen-test.servicebus.windows.net/jobs/messages/2/\
             7da9cfd5-40d5-4bb1-8d64-ec5a52e1c547"
        );
    }

    #[test]
    fn the_token_carries_the_resource_expiry_and_policy() {
        let queue = queue();
        let url = queue.queue_url.clone();

        let token = queue.sas_token(&url).expect("should sign");

        assert!(token.starts_with("SharedAccessSignature sr="));
        assert!(token.contains("&sig="));
        assert!(token.contains("&se="));
        assert!(token.ends_with("&skn=RootManageSharedAccessKey"));
        // The resource is the percent-encoded URL, as the SAS specification requires.
        assert!(token.contains("https%3A%2F%2Fskyzen-test.servicebus.windows.net%2Fjobs"));
    }

    #[test]
    fn the_signature_is_the_hmac_of_the_resource_and_the_expiry() {
        // A fixed vector: HMAC-SHA256 of "sb://skyzen-test\n1700000000" keyed with the key's own
        // characters, base64-encoded — the same bytes the legacy client signed with, which took
        // the same key the long way round through a base64 encode and decode.
        assert_eq!(
            sign(
                &SigningKey("c2t5emVuLXRlc3Qta2V5".to_owned()),
                "sb://skyzen-test",
                1_700_000_000,
            ),
            "6M1ScjAp5xMJiipz4Wiky9uTD0EvTe5MTsPwL89uVbE="
        );
    }

    #[test]
    fn neither_the_key_nor_a_token_can_be_printed_by_accident() {
        let debug = format!("{:?}", queue());

        assert!(!debug.contains("c2t5emVuLXRlc3Qta2V5"), "{debug}");
        assert!(!debug.contains("SharedAccessSignature"), "{debug}");
        assert_eq!(
            format!("{:?}", SigningKey("c2t5emVuLXRlc3Qta2V5".to_owned())),
            "SigningKey(<redacted>)"
        );
    }

    #[test]
    fn an_empty_peek_lock_ends_the_receive_and_a_failure_is_classified() {
        assert!(peeked(StatusCode::CREATED, None).unwrap());
        assert!(!peeked(StatusCode::NO_CONTENT, None).unwrap());
        assert!(matches!(
            peeked(StatusCode::TOO_MANY_REQUESTS, None),
            Err(QueueError::Throttled { .. })
        ));
        assert!(matches!(
            peeked(StatusCode::UNAUTHORIZED, None),
            Err(QueueError::Unauthorized)
        ));
    }

    #[test]
    fn a_lapsed_lease_settles_as_a_conflict() {
        assert!(settled(StatusCode::OK, None).is_ok());
        assert!(matches!(
            settled(StatusCode::NOT_FOUND, None),
            Err(QueueError::Conflict)
        ));
        // A queue that does not exist is not a lease that lapsed: it stays a backend error naming
        // the status, so a misconfiguration is not retried forever as a lost race.
        assert!(matches!(
            settled(StatusCode::GONE, None),
            Err(QueueError::Backend { .. })
        ));
        assert!(matches!(
            settled(StatusCode::TOO_MANY_REQUESTS, Some("7")),
            Err(QueueError::Throttled {
                retry_after: Some(delay)
            }) if delay == Duration::from_secs(7)
        ));
    }

    #[test]
    fn a_batch_failure_quotes_the_services_own_code() {
        let answered = |status, code: Option<&str>| ServiceBusError {
            answer: Some(ServiceAnswer {
                status,
                code: code.map(ToOwned::to_owned),
                retry_after: None,
            }),
            message: "rejected".to_owned(),
            source: None,
        };

        assert_eq!(
            batch_failure_code(&answered(
                StatusCode::FORBIDDEN,
                Some("AuthorizationFailed")
            )),
            "AuthorizationFailed"
        );
        assert_eq!(
            batch_failure_code(&answered(StatusCode::TOO_MANY_REQUESTS, None)),
            "429"
        );
        // A request that never reached the service has no code and no status of its own.
        assert_eq!(
            batch_failure_code(&ServiceBusError::local("the connection was refused")),
            "Transport"
        );
    }

    #[test]
    fn a_refusal_is_classified_the_way_the_status_reads() {
        let refused = |status, retry_after| ServiceBusError {
            answer: Some(ServiceAnswer {
                status,
                code: None,
                retry_after,
            }),
            message: "refused".to_owned(),
            source: None,
        };

        assert!(matches!(
            super::queue_error(refused(
                StatusCode::TOO_MANY_REQUESTS,
                Some(Duration::from_secs(3))
            )),
            QueueError::Throttled {
                retry_after: Some(delay)
            } if delay == Duration::from_secs(3)
        ));
        assert!(matches!(
            super::queue_error(refused(StatusCode::UNAUTHORIZED, None)),
            QueueError::Unauthorized
        ));
        assert!(matches!(
            super::queue_error(refused(StatusCode::CONFLICT, None)),
            QueueError::Conflict
        ));
        assert!(matches!(
            super::queue_error(refused(StatusCode::INTERNAL_SERVER_ERROR, None)),
            QueueError::Backend { .. }
        ));
        assert!(matches!(
            super::queue_error(ServiceBusError::local("the connection was refused")),
            QueueError::Backend { .. }
        ));
    }

    #[tokio::test]
    async fn a_delayed_retry_is_refused_rather_than_silently_immediate() {
        let error = queue()
            .nack(
                &MessageReceipt::new("{}"),
                QueueRetry::new().with_delay_seconds(30),
            )
            .await
            .expect_err("a delayed abandon should be refused");
        assert!(matches!(error, QueueError::Unsupported(_)));
    }

    #[tokio::test]
    async fn a_per_delivery_visibility_timeout_is_refused() {
        let error = queue()
            .receive(
                ReceiveOptions::new()
                    .with_wait(Duration::from_secs(30))
                    .with_visibility_timeout(Duration::from_secs(30)),
            )
            .await
            .expect_err("a per-delivery lock duration should be refused");
        assert!(matches!(error, QueueError::Unsupported(_)));
    }

    #[tokio::test]
    async fn a_receive_without_a_wait_is_refused_and_names_the_wait_to_pass() {
        // The default options carry no wait, and Service Bus has no immediate receive to answer
        // them with — so the call says so instead of blocking for the service's own default.
        let error = queue()
            .receive(ReceiveOptions::new())
            .await
            .expect_err("a receive with no wait should be refused");

        assert!(matches!(error, QueueError::Unsupported(_)));
        let message = error.to_string();
        assert!(message.contains("non-blocking"), "{message}");
        assert!(
            message.contains("ReceiveOptions::new().with_wait(Duration::from_secs(30))"),
            "{message}"
        );
    }
}
