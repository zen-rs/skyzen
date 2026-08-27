//! Azure Storage queue implementation of [`MessageQueue`].

use core::time::Duration;
use std::sync::Arc;

use azure_core::{
    credentials::TokenCredential,
    error::ErrorKind,
    http::{headers::HeaderName, RequestContent, Url},
    Error as AzureError,
};
use azure_storage_queue::{
    models::{
        QueueClientDeleteMessageOptions, QueueClientReceiveMessagesOptions,
        QueueClientSendMessageOptions, QueueClientUpdateMessageOptions, QueueMessage,
        ReceivedMessage as StorageQueueMessage,
    },
    QueueClient,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use skyzen_services::queue::{
    BatchSendFailure, MessageQueue, MessageReceipt, QueueError, QueueRetry, ReceiveOptions,
    ReceivedMessage, SendOptions,
};

use crate::status::{classify, retry_after, AzureStatus};

/// The envelope prefix marking a base64-encoded body.
const BASE64_PREFIX: &str = "skyzen-b64:";

/// The envelope prefix marking a body that is text but had to be escaped.
///
/// Only a payload that would otherwise be mistaken for an envelope carries it.
const UTF8_PREFIX: &str = "skyzen-utf8:";

/// The largest message Azure Storage queues accept, in bytes of encoded text.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// The most messages one `receive` call can take.
const MAX_RECEIVE_MESSAGES: i32 = 32;

/// The longest visibility timeout the service accepts, in seconds (seven days).
const MAX_VISIBILITY_TIMEOUT_SECONDS: i32 = 604_800;

/// An Azure Storage queue-backed message queue.
///
/// The plain, cheap Azure queue: a flat queue of text messages held in a storage account, with
/// visibility timeouts and delayed delivery. [`ServiceBusQueue`](crate::ServiceBusQueue) is the
/// richer broker; this is the one to reach for when a storage account is already there.
///
/// # Wire format
///
/// Azure Storage queues carry a message as text inside an XML document and offer no property
/// channel to tag an encoding with, so the tag travels in the body:
///
/// - Text that XML can carry, and that does not begin with one of this backend's prefixes, is sent
///   **verbatim**: JSON produced by `send_json` arrives as plain JSON, readable by any other
///   consumer of the queue.
/// - Anything else — binary, or text with characters XML 1.0 cannot represent — is sent as
///   `skyzen-b64:` followed by standard base64.
/// - Text that would itself begin with `skyzen-b64:` or `skyzen-utf8:` is sent as `skyzen-utf8:`
///   followed by the text, so it cannot be mistaken for one of the other two forms.
///
/// The mapping is injective, so [`receive`](MessageQueue::receive) returns exactly the bytes
/// [`send`](MessageQueue::send) was given.
///
/// # Platform limits
///
/// A message is capped at 64 KB of encoded text — base64 costs a third more than the bytes it
/// encodes — and a send above it is refused here rather than at the service. One `receive` takes at
/// most 32 messages, and a larger request is capped rather than split, so a consumer that wants
/// more polls again. There is no long polling: [`ReceiveOptions::wait`] is refused rather than
/// ignored.
///
/// Cloning is cheap — the client is behind an `Arc`.
#[derive(Clone)]
pub struct AzureStorageQueue {
    /// The SDK client, which carries the queue URL and its credential.
    client: Arc<QueueClient>,
}

impl core::fmt::Debug for AzureStorageQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AzureStorageQueue")
            .field("url", &self.client.url().as_str())
            .finish()
    }
}

impl AzureStorageQueue {
    /// Wrap an SDK queue client that is already configured.
    ///
    /// The escape hatch for a client this crate's own constructors cannot build — one with custom
    /// retry policies, or a credential type they do not take.
    #[must_use]
    pub fn new(client: QueueClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    /// Create a queue client authenticating with an Entra ID token credential.
    ///
    /// `account_url` is the queue endpoint of the storage account,
    /// `https://myaccount.queue.core.windows.net`. The credential comes from `azure_identity` —
    /// `DeveloperToolsCredential` for local development, `ManagedIdentityCredential` in Azure.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when `account_url` is not a URL a queue can hang off, when `queue`
    /// is not a valid queue name, or when the SDK rejects the combination — a token credential
    /// requires HTTPS.
    pub fn with_credential(
        account_url: &str,
        queue: &str,
        credential: Arc<dyn TokenCredential>,
    ) -> Result<Self, QueueError> {
        let url = queue_url(account_url, queue)?;
        let client = QueueClient::new(url, Some(credential), None).map_err(azure_error)?;
        Ok(Self::new(client))
    }

    /// Create a queue client from a queue URL that carries a shared access signature.
    ///
    /// The URL is the full queue URL with the SAS query attached, as the portal's "Generate SAS"
    /// hands it out: `https://myaccount.queue.core.windows.net/myqueue?sv=…&sig=…`.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when the URL is not a queue URL.
    pub fn from_sas_url(url: &str) -> Result<Self, QueueError> {
        let url = Url::parse(url).map_err(|error| {
            QueueError::backend_with(format!("{url:?} is not a queue URL"), error)
        })?;

        if url.query().is_none() {
            return Err(QueueError::backend(format!(
                "the queue URL {url} carries no shared access signature; \
                 use `with_credential` to authenticate with Entra ID instead"
            )));
        }

        let client = QueueClient::new(url, None, None).map_err(azure_error)?;
        Ok(Self::new(client))
    }

    /// Send one message, holding it invisible for `delay` when there is one.
    async fn send_message(&self, message: &[u8], options: SendOptions) -> Result<(), AzureError> {
        let body: RequestContent<QueueMessage, _> = QueueMessage {
            message_text: Some(encode_message(message)?),
        }
        .try_into()?;

        let visibility_timeout = options
            .delay
            .map(|delay| duration_seconds(delay, "a delivery delay"))
            .transpose()?;

        self.client
            .send_message(
                body,
                Some(QueueClientSendMessageOptions {
                    visibility_timeout,
                    ..QueueClientSendMessageOptions::default()
                }),
            )
            .await?;
        Ok(())
    }
}

/// The URL of `queue` in the storage account at `account_url`.
fn queue_url(account_url: &str, queue: &str) -> Result<Url, QueueError> {
    if !is_queue_name(queue) {
        return Err(QueueError::backend(format!(
            "{queue:?} is not an Azure Storage queue name; names are 3 to 63 characters of \
             lowercase letters, digits and hyphens"
        )));
    }

    let mut url = Url::parse(account_url).map_err(|error| {
        QueueError::backend_with(
            format!("{account_url:?} is not a storage account queue endpoint"),
            error,
        )
    })?;

    url.path_segments_mut()
        .map_err(|()| QueueError::backend(format!("{account_url:?} cannot carry a queue name")))?
        .pop_if_empty()
        .push(queue);

    Ok(url)
}

/// Whether `name` is a queue name the service accepts.
///
/// Checked here so a typo fails at construction rather than as a 400 on the first send.
fn is_queue_name(name: &str) -> bool {
    (3..=63).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Whether `c` can be carried by the XML document a queue message travels in.
///
/// Azure documents the message as having to fit "an XML request with UTF-8 encoding", which is the
/// W3C XML 1.0 character range: `#x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD] |
/// [#x10000-#x10FFFF]`. A Rust `char` is never a surrogate, so that range needs no check. This is
/// the same rule the SQS backend applies to its own bodies.
const fn is_xml_text_char(c: char) -> bool {
    matches!(c,
        '\u{9}' | '\u{A}' | '\u{D}'
        | '\u{20}'..='\u{D7FF}'
        | '\u{E000}'..='\u{FFFD}'
        | '\u{10000}'..='\u{10FFFF}')
}

/// Encode a payload into the in-band envelope, refusing one the service would reject.
fn encode_message(message: &[u8]) -> Result<String, AzureError> {
    let encoded = match core::str::from_utf8(message) {
        Ok(text) if text.chars().all(is_xml_text_char) => {
            if text.starts_with(BASE64_PREFIX) || text.starts_with(UTF8_PREFIX) {
                // Escaping a body that already looks like an envelope is what keeps the encoding
                // injective: without it, `receive` could not tell this text from an encoded blob.
                format!("{UTF8_PREFIX}{text}")
            } else {
                text.to_owned()
            }
        }
        _ => format!(
            "{BASE64_PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode(message)
        ),
    };

    if encoded.len() > MAX_MESSAGE_BYTES {
        return Err(AzureError::with_message(
            azure_core::error::ErrorKind::Other,
            format!(
                "Azure Storage queues cap a message at {MAX_MESSAGE_BYTES} bytes of encoded text; \
                 this one encodes to {}",
                encoded.len()
            ),
        ));
    }

    Ok(encoded)
}

/// Reverse [`encode_message`].
fn decode_message(text: &str) -> Result<Vec<u8>, QueueError> {
    if let Some(encoded) = text.strip_prefix(BASE64_PREFIX) {
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| {
                QueueError::backend_with(
                    format!("a message body prefixed {BASE64_PREFIX:?} is not valid base64"),
                    error,
                )
            });
    }

    Ok(text
        .strip_prefix(UTF8_PREFIX)
        .unwrap_or(text)
        .as_bytes()
        .to_vec())
}

/// The lease this backend hands back, and takes back to settle a message.
///
/// Azure Storage queues settle a message by naming both its id and the pop receipt of the delivery
/// being settled, so a receipt has to carry both.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct StorageQueueReceipt {
    /// The message's id, stable across deliveries.
    message_id: String,
    /// The pop receipt of this delivery, which the next delivery invalidates.
    pop_receipt: String,
}

impl StorageQueueReceipt {
    /// Render the receipt as the opaque token [`MessageReceipt`] carries.
    fn encode(&self) -> Result<MessageReceipt, QueueError> {
        Ok(MessageReceipt::new(serde_json::to_string(self)?))
    }

    /// Read back a receipt this backend minted.
    fn decode(receipt: &MessageReceipt) -> Result<Self, QueueError> {
        serde_json::from_str(receipt.as_str()).map_err(|error| {
            QueueError::backend_with(
                "the receipt handed to `ack`/`nack` was not minted by the Azure Storage queue \
                 backend",
                error,
            )
        })
    }
}

/// Turn one delivered message into a leased [`ReceivedMessage`].
fn received_message(message: StorageQueueMessage) -> Result<ReceivedMessage, QueueError> {
    let (Some(message_id), Some(pop_receipt)) = (message.message_id, message.pop_receipt) else {
        return Err(QueueError::backend(
            "Azure Storage delivered a message without an id and pop receipt, which can never be \
             settled",
        ));
    };

    let attempts = message
        .dequeue_count
        .map(|count| {
            u32::try_from(count).map_err(|error| {
                QueueError::backend_with(
                    format!(
                        "Azure Storage reported a DequeueCount of {count}, which is not a count"
                    ),
                    error,
                )
            })
        })
        .transpose()?;

    Ok(ReceivedMessage {
        body: decode_message(&message.message_text.unwrap_or_default())?,
        receipt: StorageQueueReceipt {
            message_id: message_id.clone(),
            pop_receipt,
        }
        .encode()?,
        id: Some(message_id),
        attempts,
    })
}

/// Render a duration as the whole seconds an Azure Storage queue parameter takes.
///
/// `parameter` names the knob in the error, so a caller who asks for more than the service accepts
/// learns which one and which ceiling instead of getting a validation failure back off the wire.
fn duration_seconds(duration: Duration, parameter: &str) -> Result<i32, AzureError> {
    let mut seconds = duration.as_secs();
    if duration.subsec_nanos() > 0 {
        seconds = seconds.saturating_add(1);
    }

    i32::try_from(seconds)
        .ok()
        .filter(|seconds| *seconds <= MAX_VISIBILITY_TIMEOUT_SECONDS)
        .ok_or_else(|| {
            AzureError::with_message(
                azure_core::error::ErrorKind::Other,
                format!(
                    "Azure Storage queues cap {parameter} at {MAX_VISIBILITY_TIMEOUT_SECONDS} \
                     seconds; {seconds} was requested"
                ),
            )
        })
}

/// The `numofmessages` for a requested batch size.
///
/// A request above the service's cap of 32 is capped rather than refused: a consumer that wants
/// more polls again, and chunking into several calls would lease messages the caller could not
/// settle if a later call failed. Zero is refused, because an empty result would be
/// indistinguishable from an empty queue.
fn receive_batch_size(max_messages: usize) -> Result<i32, QueueError> {
    if max_messages == 0 {
        return Err(QueueError::backend(
            "ReceiveOptions::max_messages must be at least 1; asking Azure Storage for zero \
             messages would look the same as an empty queue",
        ));
    }

    Ok(
        i32::try_from(max_messages).map_or(MAX_RECEIVE_MESSAGES, |requested| {
            requested.min(MAX_RECEIVE_MESSAGES)
        }),
    )
}

/// The visibility timeout that carries out a [`QueueRetry`].
///
/// Making a message visible again *is* the retry: the timeout counts from now, so zero redelivers
/// immediately and a delay defers redelivery by exactly that long.
fn retry_visibility_seconds(retry: QueueRetry) -> Result<i32, QueueError> {
    let Some(delay_seconds) = retry.delay_seconds else {
        return Ok(0);
    };

    i32::try_from(delay_seconds)
        .ok()
        .filter(|seconds| *seconds <= MAX_VISIBILITY_TIMEOUT_SECONDS)
        .ok_or_else(|| {
            QueueError::backend(format!(
                "Azure Storage queues cap a redelivery delay at \
                 {MAX_VISIBILITY_TIMEOUT_SECONDS} seconds; {delay_seconds} was requested"
            ))
        })
}

/// How long a throttled request was told to wait, when the response said.
fn throttle_delay(error: &AzureError) -> Option<Duration> {
    match error.kind() {
        ErrorKind::HttpResponse {
            raw_response: Some(response),
            ..
        } => response
            .headers()
            .get_optional_str(&HeaderName::from_static("retry-after"))
            .and_then(retry_after),
        _ => None,
    }
}

/// Map an SDK error onto the portable taxonomy, keeping its source chain.
fn azure_error(error: AzureError) -> QueueError {
    match error
        .http_status()
        .map(|status| classify(u16::from(status)))
    {
        Some(AzureStatus::Throttled) => QueueError::Throttled {
            retry_after: throttle_delay(&error),
        },
        Some(AzureStatus::Unauthorized) => QueueError::Unauthorized,
        Some(AzureStatus::Conflict) => QueueError::Conflict,
        _ => QueueError::backend_with(error.to_string(), error),
    }
}

/// Map an error from settling a lease, turning a lapsed lease into [`QueueError::Conflict`].
///
/// A pop receipt names one delivery. Once the visibility timeout lapses the message is delivered
/// again with a new receipt and the old one is refused with `404` — the message state changing
/// underneath the call, which is what [`QueueError::Conflict`] documents, not a backend failure.
fn settle_error(error: AzureError) -> QueueError {
    match error
        .http_status()
        .map(|status| classify(u16::from(status)))
    {
        Some(AzureStatus::Absent | AzureStatus::Conflict) => QueueError::Conflict,
        _ => azure_error(error),
    }
}

/// The service's own code for a rejected batch entry.
fn batch_failure_code(error: &AzureError) -> String {
    error.http_status().map_or_else(
        || format!("{:?}", error.kind()),
        |status| u16::from(status).to_string(),
    )
}

impl MessageQueue for AzureStorageQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        self.send_message(message, SendOptions::new())
            .await
            .map_err(azure_error)
    }

    /// Send one message, holding it invisible for [`SendOptions::delay`] when there is one.
    ///
    /// A delay becomes the message's initial visibility timeout, which is how Azure Storage queues
    /// schedule deferred work.
    async fn send_with(&self, message: &[u8], options: SendOptions) -> Result<(), QueueError> {
        self.send_message(message, options)
            .await
            .map_err(azure_error)
    }

    /// Send messages sequentially: Azure Storage queues have no batch send.
    ///
    /// Not atomic. Every message the batch carried whose index is absent from
    /// [`QueueError::PartialBatch`] was enqueued, and re-sending only the named failures is what
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
    /// [`ReceiveOptions::wait`] is refused: Azure Storage queues answer immediately with whatever
    /// is there, and there is no long poll to map it onto.
    async fn receive(&self, options: ReceiveOptions) -> Result<Vec<ReceivedMessage>, QueueError> {
        if options.wait.is_some() {
            return Err(QueueError::Unsupported(
                "Azure Storage queues have no long polling; a consumer polls `receive` on its own \
                 schedule, or uses Service Bus, which does",
            ));
        }

        let visibility_timeout = options
            .visibility_timeout
            .map(|timeout| duration_seconds(timeout, "a visibility timeout"))
            .transpose()
            .map_err(azure_error)?;

        let response = self
            .client
            .receive_messages(Some(QueueClientReceiveMessagesOptions {
                number_of_messages: Some(receive_batch_size(options.max_messages)?),
                visibility_timeout,
                ..QueueClientReceiveMessagesOptions::default()
            }))
            .await
            .map_err(azure_error)?;

        response
            .into_model()
            .map_err(azure_error)?
            .items
            .unwrap_or_default()
            .into_iter()
            .map(received_message)
            .collect()
    }

    /// Delete a leased message, which is how Azure Storage queues acknowledge one.
    async fn ack(&self, receipt: &MessageReceipt) -> Result<(), QueueError> {
        let receipt = StorageQueueReceipt::decode(receipt)?;

        self.client
            .delete_message(
                &receipt.message_id,
                &receipt.pop_receipt,
                Some(QueueClientDeleteMessageOptions::default()),
            )
            .await
            .map_err(settle_error)?;
        Ok(())
    }

    /// Return a message to the queue by resetting its visibility timeout.
    ///
    /// [`QueueRetry::delay_seconds`] becomes the new timeout, so `None` redelivers as soon as the
    /// service can and a delay holds the message invisible for exactly that long first.
    async fn nack(&self, receipt: &MessageReceipt, retry: QueueRetry) -> Result<(), QueueError> {
        let receipt = StorageQueueReceipt::decode(receipt)?;

        self.client
            .update_message(
                &receipt.message_id,
                &receipt.pop_receipt,
                retry_visibility_seconds(retry)?,
                Some(QueueClientUpdateMessageOptions::default()),
            )
            .await
            .map_err(settle_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_message, encode_message, is_queue_name, queue_url, receive_batch_size,
        retry_visibility_seconds, AzureStorageQueue, StorageQueueReceipt, BASE64_PREFIX,
        MAX_MESSAGE_BYTES, MAX_RECEIVE_MESSAGES, UTF8_PREFIX,
    };
    use skyzen_services::queue::{MessageQueue, MessageReceipt, QueueError, QueueRetry};

    /// The round trip the wire format promises: what `send` encodes, `receive` decodes.
    fn round_trip(payload: &[u8]) -> Vec<u8> {
        decode_message(&encode_message(payload).expect("payload should encode"))
            .expect("payload should decode")
    }

    #[test]
    fn json_passes_through_unchanged() {
        let payload = br#"{"kind":"email"}"#;
        assert_eq!(encode_message(payload).unwrap(), r#"{"kind":"email"}"#);
        assert_eq!(round_trip(payload), payload.to_vec());
    }

    #[test]
    fn unicode_text_passes_through_unchanged() {
        let payload = "hello 世界".as_bytes();
        assert_eq!(encode_message(payload).unwrap(), "hello 世界");
        assert_eq!(round_trip(payload), payload.to_vec());
    }

    #[test]
    fn binary_is_base64_encoded_behind_the_prefix() {
        let payload = [0xFF, 0xFE, 0x00, 0x01];
        let encoded = encode_message(&payload).unwrap();
        assert!(encoded.starts_with(BASE64_PREFIX));
        assert_eq!(round_trip(&payload), payload.to_vec());
    }

    #[test]
    fn text_xml_cannot_carry_is_base64_encoded() {
        // A lone form feed is valid UTF-8 and invalid XML 1.0, so it cannot travel verbatim.
        let payload = b"before\x0Cafter";
        let encoded = encode_message(payload).unwrap();
        assert!(encoded.starts_with(BASE64_PREFIX));
        assert_eq!(round_trip(payload), payload.to_vec());
    }

    #[test]
    fn text_that_looks_like_an_envelope_is_escaped_so_the_encoding_stays_injective() {
        for payload in [
            format!("{BASE64_PREFIX}aGVsbG8="),
            format!("{UTF8_PREFIX}hello"),
        ] {
            let encoded = encode_message(payload.as_bytes()).unwrap();
            assert_eq!(encoded, format!("{UTF8_PREFIX}{payload}"));
            assert_eq!(round_trip(payload.as_bytes()), payload.as_bytes().to_vec());
        }
    }

    #[test]
    fn a_body_that_merely_looks_base64_is_not_decoded() {
        let payload = b"aGVsbG8=";
        assert_eq!(round_trip(payload), payload.to_vec());
    }

    #[test]
    fn a_message_beyond_the_platform_cap_is_refused_before_it_is_sent() {
        let payload = vec![b'a'; MAX_MESSAGE_BYTES + 1];
        let error = encode_message(&payload).expect_err("an oversized message should be refused");
        assert!(error.to_string().contains("65536"));
    }

    #[test]
    fn a_body_prefixed_base64_that_is_not_base64_is_refused() {
        let error = decode_message(&format!("{BASE64_PREFIX}not base64!"))
            .expect_err("a malformed envelope should be refused");
        assert!(error.to_string().contains("base64"));
    }

    #[test]
    fn receipts_round_trip_through_their_opaque_token() {
        let receipt = StorageQueueReceipt {
            message_id: "5fe8e1d0-3b06-4a1e-9f21-8f3b1a0a5f21".to_owned(),
            pop_receipt: "AgAAAAMAAAAAAAAAqp3n0y7d1gE=".to_owned(),
        };

        let encoded = receipt.encode().expect("receipt should encode");
        assert_eq!(StorageQueueReceipt::decode(&encoded).unwrap(), receipt);
    }

    #[test]
    fn a_receipt_from_another_backend_is_refused() {
        let error = StorageQueueReceipt::decode(&MessageReceipt::new("sqs-receipt-handle"))
            .expect_err("a foreign receipt should not decode");
        assert!(error.to_string().contains("not minted"));
    }

    #[test]
    fn a_queue_url_hangs_the_queue_off_the_account_endpoint() {
        let url = queue_url("https://skyzentest.queue.core.windows.net", "jobs")
            .expect("should build a queue URL");
        assert_eq!(
            url.as_str(),
            "https://skyzentest.queue.core.windows.net/jobs"
        );

        let url = queue_url("https://skyzentest.queue.core.windows.net/", "jobs")
            .expect("a trailing slash should not double up");
        assert_eq!(
            url.as_str(),
            "https://skyzentest.queue.core.windows.net/jobs"
        );
    }

    #[test]
    fn queue_names_the_service_would_reject_are_refused_at_construction() {
        assert!(is_queue_name("jobs"));
        assert!(is_queue_name("jobs-2"));
        assert!(!is_queue_name("ab"));
        assert!(!is_queue_name("Jobs"));
        assert!(!is_queue_name("jobs_2"));
        assert!(!is_queue_name("-jobs"));
        assert!(!is_queue_name("jobs-"));
        assert!(queue_url("https://skyzentest.queue.core.windows.net", "Jobs").is_err());
    }

    #[test]
    fn a_queue_url_without_a_signature_is_refused_rather_than_failing_unauthenticated() {
        let error =
            AzureStorageQueue::from_sas_url("https://skyzentest.queue.core.windows.net/jobs")
                .expect_err("an unsigned URL should be refused");
        assert!(error.to_string().contains("shared access signature"));
    }

    #[test]
    fn a_batch_size_caps_at_the_platform_limit_and_refuses_zero() {
        assert_eq!(receive_batch_size(1).unwrap(), 1);
        assert_eq!(receive_batch_size(32).unwrap(), MAX_RECEIVE_MESSAGES);
        assert_eq!(receive_batch_size(500).unwrap(), MAX_RECEIVE_MESSAGES);
        assert!(receive_batch_size(0).is_err());
    }

    #[test]
    fn a_retry_without_a_delay_makes_the_message_visible_immediately() {
        assert_eq!(retry_visibility_seconds(QueueRetry::new()).unwrap(), 0);
        assert_eq!(
            retry_visibility_seconds(QueueRetry::new().with_delay_seconds(30)).unwrap(),
            30
        );
        assert!(matches!(
            retry_visibility_seconds(QueueRetry::new().with_delay_seconds(u32::MAX)),
            Err(QueueError::Backend { .. })
        ));
    }

    #[tokio::test]
    async fn long_polling_is_refused_rather_than_silently_returning_empty() {
        let queue = AzureStorageQueue::from_sas_url(
            "https://skyzentest.queue.core.windows.net/jobs?sv=2020-12-06&sig=signature",
        )
        .expect("should build");

        let error = queue
            .receive(
                skyzen_services::queue::ReceiveOptions::new()
                    .with_wait(core::time::Duration::from_secs(20)),
            )
            .await
            .expect_err("a long poll should be refused");
        assert!(matches!(error, QueueError::Unsupported(_)));
    }
}
