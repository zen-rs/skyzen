//! Azure Service Bus implementation of [`MessageQueue`].

use core::time::Duration;
use std::{collections::HashMap, sync::Arc};

use azure_core_legacy::{
    auth::Secret,
    error::{Error as AzureError, ErrorKind},
    headers::{HeaderName, Headers},
    hmac::hmac_sha256,
    new_http_client, HttpClient, Method, Request, StatusCode, Url,
};
use azure_messaging_servicebus::service_bus::{
    PeekLockResponse, QueueClient, SendMessageOptions, SettableBrokerProperties,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use skyzen_services::queue::{
    BatchSendFailure, MessageQueue, MessageReceipt, QueueError, QueueRetry, ReceiveOptions,
    ReceivedMessage, SendOptions,
};
use time::OffsetDateTime;

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

/// How long the SAS token minted for one settlement request stays valid.
///
/// The token authorizes a single `DELETE` or `PUT` that is issued immediately; a short life keeps
/// a leaked token useless rather than granting queue access for an hour.
const SETTLE_TOKEN_TTL: Duration = Duration::from_secs(300);

/// The longest server-side wait the Service Bus REST peek-lock accepts.
const MAX_PEEK_LOCK_WAIT: Duration = Duration::from_secs(55);

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
/// Cloning is cheap — the client and its HTTP stack are behind `Arc`.
#[derive(Debug, Clone)]
pub struct ServiceBusQueue {
    /// The SDK client, which owns sending and peek-lock receiving.
    client: QueueClient,
    /// The HTTP stack the client uses, shared so settlement rides the same connection pool.
    http_client: Arc<dyn HttpClient>,
    /// `https://{namespace}.servicebus.windows.net/{queue}`, the root of every settlement URL.
    queue_url: Url,
    /// The shared-access policy name that signs settlement requests.
    policy_name: String,
    /// The signing key, base64-encoded the way [`hmac_sha256`] expects to receive it.
    signing_key: Secret,
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
    /// URL, which the legacy SDK would otherwise interpolate unencoded.
    pub fn new(
        namespace: &str,
        queue: &str,
        policy_name: impl Into<String>,
        signing_key: &str,
    ) -> Result<Self, QueueError> {
        let queue_url = queue_url(namespace, queue)?;
        let http_client = new_http_client();
        let policy_name = policy_name.into();

        let client = QueueClient::new(
            Arc::clone(&http_client),
            namespace,
            queue,
            policy_name.clone(),
            signing_key.to_owned(),
        )
        .map_err(backend_error)?;

        Ok(Self {
            client,
            http_client,
            queue_url,
            policy_name,
            // `hmac_sha256` base64-decodes the key it is handed, and a Service Bus key signs with
            // its own characters as the key bytes — so it is encoded once here to survive that
            // decode. This is what the SDK's own client does with the key it is given.
            signing_key: Secret::new(azure_core_legacy::base64::encode(signing_key)),
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
    /// key, which this client cannot re-sign settlement requests with.
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

    /// Send one already-encoded message, reporting the SDK's own error.
    async fn send_message(&self, message: &[u8], options: SendOptions) -> Result<(), AzureError> {
        let encoded = encode_message(message);
        let scheduled = options.delay.map(scheduled_enqueue_time).transpose()?;

        let send_options = SendMessageOptions {
            content_type: None,
            broker_properties: scheduled.map(|scheduled_enqueue_time_utc| {
                SettableBrokerProperties {
                    scheduled_enqueue_time_utc: Some(scheduled_enqueue_time_utc),
                    ..SettableBrokerProperties::default()
                }
            }),
            custom_properties: encoded.base64.then(|| {
                HashMap::from([(
                    CONTENT_ENCODING_PROPERTY.to_owned(),
                    BASE64_ENCODING.to_owned(),
                )])
            }),
        };

        self.client
            .send_message(&encoded.body, Some(send_options))
            .await
    }

    /// Build a settlement request against `receipt`'s lock, signed for [`SETTLE_TOKEN_TTL`].
    ///
    /// The URL is the one the Service Bus REST API documents for settling a peek-locked message,
    /// `…/{queue}/messages/{messageId}/{lockToken}`: `DELETE` completes it, `PUT` unlocks it. The
    /// SDK reaches it only through the response object a receive returns, which cannot outlive the
    /// call, so a receipt that a caller may settle minutes later has to name it directly.
    fn settle_request(
        &self,
        method: Method,
        receipt: &ServiceBusReceipt,
    ) -> Result<Request, QueueError> {
        let mut url = self.queue_url.clone();
        url.path_segments_mut()
            .map_err(|()| QueueError::backend("the Service Bus queue URL cannot carry a path"))?
            .extend(["messages", &receipt.message_id, &receipt.lock_token]);

        let mut request = Request::new(url.clone(), method);
        request.insert_header(
            azure_core_legacy::headers::AUTHORIZATION,
            self.sas_token(&url)?,
        );
        request.insert_header(azure_core_legacy::headers::CONTENT_LENGTH, "0");
        request.set_body(azure_core_legacy::EMPTY_BODY);
        Ok(request)
    }

    /// Mint a shared-access-signature token authorizing one request against `url`.
    ///
    /// The token is the documented Service Bus SAS: an HMAC-SHA256 over the percent-encoded
    /// resource URI and the expiry instant, keyed with the policy's key.
    fn sas_token(&self, url: &Url) -> Result<String, QueueError> {
        let resource: String =
            url::form_urlencoded::byte_serialize(url.as_str().as_bytes()).collect();
        let expiry = OffsetDateTime::now_utc()
            .checked_add(
                time::Duration::try_from(SETTLE_TOKEN_TTL)
                    .map_err(|error| QueueError::backend_with("SAS token lifetime", error))?,
            )
            .ok_or_else(|| QueueError::backend("the SAS token expiry overflowed the calendar"))?
            .unix_timestamp();

        let signature = hmac_sha256(&format!("{resource}\n{expiry}"), &self.signing_key)
            .map_err(backend_error)?;
        let signature: String =
            url::form_urlencoded::byte_serialize(signature.as_bytes()).collect();

        Ok(format!(
            "SharedAccessSignature sr={resource}&sig={signature}&se={expiry}&skn={}",
            self.policy_name
        ))
    }

    /// Issue a settlement request and read the outcome off its status.
    async fn settle(&self, method: Method, receipt: &MessageReceipt) -> Result<(), QueueError> {
        let receipt = ServiceBusReceipt::decode(receipt)?;
        let request = self.settle_request(method, &receipt)?;

        let response = self
            .http_client
            .execute_request(&request)
            .await
            .map_err(backend_error)?;

        let retry_after_header = response
            .headers()
            .get_optional_string(&azure_core_legacy::headers::RETRY_AFTER);

        settled(response.status(), retry_after_header.as_deref())
    }
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
/// what makes it safe for the legacy SDK to interpolate the name straight into its URLs.
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
                     sign the settlement requests `ack` and `nack` issue; use a connection string \
                     with a SharedAccessKey",
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
/// A host outside `.servicebus.windows.net` is refused rather than accepted and then ignored: the
/// legacy SDK builds every request URL against the public-cloud host, so a sovereign-cloud endpoint
/// would silently address the wrong namespace.
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
                 the Service Bus client this backend uses addresses the public cloud only"
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

/// The `skyzen-content-encoding` property, read off a delivered message's custom properties.
///
/// The SDK surfaces custom properties only as "anything that can be built from the response
/// headers", which is what this is.
struct ContentEncodingProperty(Option<String>);

impl From<Headers> for ContentEncodingProperty {
    fn from(headers: Headers) -> Self {
        Self(headers.get_optional_string(&HeaderName::from_static(CONTENT_ENCODING_PROPERTY)))
    }
}

/// The lease this backend hands back, and takes back to settle a message.
///
/// Service Bus settles a peek-locked message by naming both its id and its lock token in the
/// request URL, so a receipt has to carry both.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ServiceBusReceipt {
    /// The `MessageId` broker property.
    message_id: String,
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

/// The instant `delay` from now, for Service Bus' `ScheduledEnqueueTimeUtc` broker property.
fn scheduled_enqueue_time(delay: Duration) -> Result<OffsetDateTime, AzureError> {
    let delay = time::Duration::try_from(delay).map_err(|error| {
        AzureError::full(
            ErrorKind::DataConversion,
            error,
            "the requested delivery delay does not fit a Service Bus schedule",
        )
    })?;

    OffsetDateTime::now_utc().checked_add(delay).ok_or_else(|| {
        AzureError::message(
            ErrorKind::DataConversion,
            "the requested delivery delay overflowed the calendar",
        )
    })
}

/// Turn one peek-lock response into a leased message.
fn received_message(response: &PeekLockResponse) -> Result<ReceivedMessage, QueueError> {
    let broker_properties = response.broker_properties().ok_or_else(|| {
        QueueError::backend(
            "Service Bus delivered a message with no BrokerProperties, which can never be settled",
        )
    })?;

    let ContentEncodingProperty(encoding) = match response.custom_properties() {
        Ok(property) => property,
        Err(never) => match never {},
    };

    let receipt = ServiceBusReceipt {
        message_id: broker_properties.message_id.clone(),
        lock_token: broker_properties.lock_token.clone(),
    };

    Ok(ReceivedMessage {
        id: Some(broker_properties.message_id.clone()),
        body: decode_message(&response.body(), encoding.as_deref())?,
        receipt: receipt.encode()?,
        attempts: Some(
            u32::try_from(broker_properties.delivery_count).map_err(|error| {
                QueueError::backend_with(
                    format!(
                        "Service Bus reported a DeliveryCount of {}, which is not a count",
                        broker_properties.delivery_count
                    ),
                    error,
                )
            })?,
        ),
    })
}

/// The peek-lock wait to ask the service for on the `index`-th fetch of one receive.
///
/// Only the first fetch waits: the caller asked to wait up to `wait` for *a* message, not up to
/// `wait` for each of them, so every later fetch takes what is already there and stops at the first
/// empty answer.
fn peek_lock_wait(index: usize, wait: Option<Duration>) -> Result<Option<Duration>, QueueError> {
    if index > 0 {
        return Ok(Some(Duration::ZERO));
    }

    match wait {
        Some(wait) if wait > MAX_PEEK_LOCK_WAIT => Err(QueueError::backend(format!(
            "Service Bus caps a peek-lock wait at {} seconds; {} was requested",
            MAX_PEEK_LOCK_WAIT.as_secs(),
            wait.as_secs()
        ))),
        wait => Ok(wait),
    }
}

/// Whether a peek-lock answered with a message, and refuse anything but a message or an empty queue.
fn peeked(status: StatusCode) -> Result<bool, QueueError> {
    match status {
        StatusCode::Ok | StatusCode::Created => Ok(true),
        // Service Bus answers a peek-lock that waited out its timeout with an empty 204.
        StatusCode::NoContent => Ok(false),
        status => Err(status_error(status, "peek-lock", None)),
    }
}

/// Read the outcome of a settlement request off its status.
fn settled(status: StatusCode, retry_after_header: Option<&str>) -> Result<(), QueueError> {
    if status.is_success() {
        return Ok(());
    }

    match classify(u16::from(status)) {
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
    match classify(u16::from(status)) {
        AzureStatus::Throttled => QueueError::Throttled {
            retry_after: retry_after_header.and_then(retry_after),
        },
        AzureStatus::Unauthorized => QueueError::Unauthorized,
        AzureStatus::Conflict => QueueError::Conflict,
        _ => QueueError::backend(format!(
            "Service Bus answered a {operation} request with {}",
            u16::from(status)
        )),
    }
}

/// The HTTP status an SDK error carries, when it failed against the service at all.
fn http_status(error: &AzureError) -> Option<StatusCode> {
    match error.kind() {
        ErrorKind::HttpResponse { status, .. } => Some(*status),
        _ => None,
    }
}

/// Map an SDK error onto the portable taxonomy, keeping its source chain.
fn sdk_error(error: AzureError) -> QueueError {
    match http_status(&error).map(|status| classify(u16::from(status))) {
        Some(AzureStatus::Throttled) => QueueError::Throttled { retry_after: None },
        Some(AzureStatus::Unauthorized) => QueueError::Unauthorized,
        Some(AzureStatus::Conflict) => QueueError::Conflict,
        _ => backend_error(error),
    }
}

/// The service's own code for a rejected batch entry.
///
/// Service Bus has no batch send, so there is no per-entry code to quote: the closest thing the
/// service says about one rejected message is the status (and error code, when it sends one) of
/// that message's own request.
fn batch_failure_code(error: &AzureError) -> String {
    match error.kind() {
        ErrorKind::HttpResponse { status, error_code } => error_code
            .clone()
            .unwrap_or_else(|| u16::from(*status).to_string()),
        kind => format!("{kind:?}"),
    }
}

/// Map any error with a source chain onto [`QueueError::Backend`].
fn backend_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> QueueError {
    QueueError::backend_with(error.to_string(), error)
}

impl MessageQueue for ServiceBusQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        self.send_message(message, SendOptions::new())
            .await
            .map_err(sdk_error)
    }

    /// Send one message, scheduling it when [`SendOptions::delay`] asks for one.
    ///
    /// A delay becomes Service Bus' `ScheduledEnqueueTimeUtc` broker property, so the message sits
    /// in the queue invisible until that instant.
    async fn send_with(&self, message: &[u8], options: SendOptions) -> Result<(), QueueError> {
        self.send_message(message, options).await.map_err(sdk_error)
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
    /// Service Bus locks one message per peek-lock request, so a batch is that many requests; the
    /// first may wait up to [`ReceiveOptions::wait`] for a message to arrive and the rest take only
    /// what is already there, stopping at the first empty answer.
    ///
    /// [`ReceiveOptions::visibility_timeout`] is refused: the lease lasts the queue's configured
    /// `LockDuration`, and the REST API has no way to override it for one delivery.
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

        let mut messages = Vec::with_capacity(options.max_messages);
        for index in 0..options.max_messages {
            let response = self
                .client
                .peek_lock_message2(peek_lock_wait(index, options.wait)?)
                .await
                .map_err(sdk_error)?;

            if !peeked(*response.status())? {
                break;
            }

            messages.push(received_message(&response)?);
        }

        Ok(messages)
    }

    /// Complete a leased message, which deletes it from the queue.
    async fn ack(&self, receipt: &MessageReceipt) -> Result<(), QueueError> {
        self.settle(Method::Delete, receipt).await
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

        self.settle(Method::Put, receipt).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        batch_failure_code, decode_message, encode_message, peek_lock_wait, peeked, queue_url,
        settled, ConnectionString, ServiceBusQueue, ServiceBusReceipt, MAX_PEEK_LOCK_WAIT,
    };
    use azure_core_legacy::{
        error::{Error as AzureError, ErrorKind},
        StatusCode,
    };
    use base64::Engine as _;
    use core::time::Duration;
    use skyzen_services::queue::{MessageQueue, MessageReceipt, QueueError, QueueRetry};

    const CONNECTION_STRING: &str = "Endpoint=sb://skyzen-test.servicebus.windows.net/;\
         SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2t5emVuLXRlc3Qta2V5";

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
    fn receipts_round_trip_through_their_opaque_token() {
        let receipt = ServiceBusReceipt {
            message_id: "1c9b4a1e-0b6d-4b39-9c1c-6d8f0c3b7f21".to_owned(),
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
    fn a_pre_minted_signature_is_refused_because_settlement_cannot_be_signed() {
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
        let queue = ServiceBusQueue::from_connection_string(CONNECTION_STRING, "jobs")
            .expect("should build");
        let receipt = ServiceBusReceipt {
            message_id: "message-1".to_owned(),
            lock_token: "lock-1".to_owned(),
        };

        let request = queue
            .settle_request(azure_core_legacy::Method::Delete, &receipt)
            .expect("should build a settle request");

        assert_eq!(
            request.url().as_str(),
            "https://skyzen-test.servicebus.windows.net/jobs/messages/message-1/lock-1"
        );
        assert_eq!(*request.method(), azure_core_legacy::Method::Delete);
    }

    #[test]
    fn the_settlement_token_carries_the_resource_expiry_and_policy() {
        let queue = ServiceBusQueue::from_connection_string(CONNECTION_STRING, "jobs")
            .expect("should build");
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
    fn only_the_first_fetch_of_a_receive_waits() {
        let wait = Some(Duration::from_secs(20));
        assert_eq!(peek_lock_wait(0, wait).unwrap(), wait);
        assert_eq!(peek_lock_wait(1, wait).unwrap(), Some(Duration::ZERO));
        assert_eq!(peek_lock_wait(0, None).unwrap(), None);
    }

    #[test]
    fn a_wait_beyond_the_platform_cap_is_refused() {
        let error = peek_lock_wait(0, Some(MAX_PEEK_LOCK_WAIT + Duration::from_secs(1)))
            .expect_err("an oversized wait should be refused");
        assert!(error.to_string().contains("55"));
    }

    #[test]
    fn an_empty_peek_lock_ends_the_receive_and_a_failure_is_classified() {
        assert!(peeked(StatusCode::Created).unwrap());
        assert!(!peeked(StatusCode::NoContent).unwrap());
        assert!(matches!(
            peeked(StatusCode::TooManyRequests),
            Err(QueueError::Throttled { .. })
        ));
        assert!(matches!(
            peeked(StatusCode::Unauthorized),
            Err(QueueError::Unauthorized)
        ));
    }

    #[test]
    fn a_lapsed_lease_settles_as_a_conflict() {
        assert!(settled(StatusCode::Ok, None).is_ok());
        assert!(matches!(
            settled(StatusCode::Gone, None),
            Err(QueueError::Conflict)
        ));
        assert!(matches!(
            settled(StatusCode::NotFound, None),
            Err(QueueError::Conflict)
        ));
        assert!(matches!(
            settled(StatusCode::TooManyRequests, Some("7")),
            Err(QueueError::Throttled {
                retry_after: Some(delay)
            }) if delay == Duration::from_secs(7)
        ));
    }

    #[test]
    fn a_batch_failure_quotes_the_services_own_code() {
        let error = AzureError::message(
            ErrorKind::http_response(
                StatusCode::Forbidden,
                Some("AuthorizationFailed".to_owned()),
            ),
            "rejected",
        );
        assert_eq!(batch_failure_code(&error), "AuthorizationFailed");

        let error = AzureError::message(
            ErrorKind::http_response(StatusCode::TooManyRequests, None),
            "rejected",
        );
        assert_eq!(batch_failure_code(&error), "429");
    }

    #[tokio::test]
    async fn a_delayed_retry_is_refused_rather_than_silently_immediate() {
        let queue = ServiceBusQueue::from_connection_string(CONNECTION_STRING, "jobs")
            .expect("should build");

        let error = queue
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
        let queue = ServiceBusQueue::from_connection_string(CONNECTION_STRING, "jobs")
            .expect("should build");

        let error = queue
            .receive(
                skyzen_services::queue::ReceiveOptions::new()
                    .with_visibility_timeout(Duration::from_secs(30)),
            )
            .await
            .expect_err("a per-delivery lock duration should be refused");
        assert!(matches!(error, QueueError::Unsupported(_)));
    }
}
