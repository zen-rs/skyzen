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
use serde::{Deserialize, Serialize};
use skyzen_services::queue::{
    envelope, BatchSendFailure, MessageQueue, MessageReceipt, QueueError, QueueRetry,
    ReceiveOptions, ReceivedMessage, SendOptions,
};

use crate::status::{classify, retry_after, AzureStatus};

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
/// channel to tag an encoding with, so the tag travels in the body, in the shared
/// [`queue::envelope`](skyzen_services::queue::envelope) format: XML-safe text verbatim, anything
/// else behind `skyzen-b64:`, and text that would itself look like an envelope behind
/// `skyzen-utf8:`. The mapping is injective, so [`receive`](MessageQueue::receive) returns exactly
/// the bytes [`send`](MessageQueue::send) was given.
///
/// The format lives in `skyzen-services` rather than here because the framework's Azure Functions
/// integration reads it back off messages the *host* delivers, and a platform crate never depends
/// on the framework crate.
///
/// # Platform limits
///
/// A message is capped at 64 KB of encoded text — base64 costs a third more than the bytes it
/// encodes — and a send above it is refused here rather than at the service. One `receive` takes at
/// most 32 messages, and a larger request is capped rather than split, so a consumer that wants
/// more polls again.
///
/// The service has no long polling of its own, so [`ReceiveOptions::wait`] is *emulated*: the
/// queue is polled at [`POLL_INTERVAL`] until a message arrives or the wait elapses. Emulating it
/// here rather than in each consumer is what lets one portable consumer loop drive this backend
/// and a genuinely long-polling one (SQS, Service Bus) with the same options.
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

    /// Create a queue client from the signed queue URL held in an environment variable.
    ///
    /// The counterpart of the `from_env` every other backend in this workspace has. There is no
    /// fixed variable name to default to, because the whole URL — account, queue and signature —
    /// is the credential: a deployment holding two queues holds two unrelated URLs, so the caller
    /// (or a `[native.service.<name>]` wiring's `sas_url_env`) names the one it means.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] naming `variable` when it is unset, and when its value is not a
    /// signed queue URL — see [`from_sas_url`](Self::from_sas_url).
    pub fn from_sas_env(variable: &str) -> Result<Self, QueueError> {
        let url = std::env::var(variable).map_err(|error| {
            QueueError::backend_with(
                format!("{variable} is not set to a signed Azure Storage queue URL"),
                error,
            )
        })?;

        Self::from_sas_url(&url).map_err(|error| {
            QueueError::backend_with(
                format!("{variable} does not hold a usable Azure Storage queue URL"),
                error,
            )
        })
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

    /// One trip to the service for whatever is visible right now.
    async fn receive_once(
        &self,
        options: ReceiveOptions,
    ) -> Result<Vec<ReceivedMessage>, QueueError> {
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
}

/// How often an emulated long poll asks the service again.
///
/// A second is short enough that a message is picked up promptly and long enough that waiting out
/// a twenty-second poll costs twenty requests rather than thousands.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Turn a series of immediate receives into one that waits.
///
/// Returns as soon as `receive_once` yields anything, and empty once `wait` has elapsed — the
/// contract a long-polling backend offers natively. A failure is returned rather than retried: the
/// caller's own backoff is the right place to decide what a broken queue means.
async fn long_poll<Poll, Fut>(
    wait: Duration,
    mut receive_once: Poll,
) -> Result<Vec<ReceivedMessage>, QueueError>
where
    Poll: FnMut() -> Fut,
    Fut: core::future::Future<Output = Result<Vec<ReceivedMessage>, QueueError>>,
{
    let deadline = std::time::Instant::now() + wait;

    loop {
        let messages = receive_once().await?;
        if !messages.is_empty() {
            return Ok(messages);
        }

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(Vec::new());
        }

        async_io::Timer::after(POLL_INTERVAL.min(remaining)).await;
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

/// Encode a payload into the in-band envelope, refusing one the service would reject.
///
/// The envelope itself is [`skyzen_services::queue::envelope`]; the only thing added here is the
/// platform's size cap, which applies to the encoded text.
fn encode_message(message: &[u8]) -> Result<String, AzureError> {
    let encoded = envelope::encode(message);

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
    envelope::decode(text).map_err(|error| {
        QueueError::backend_with("a queue message is not a Skyzen envelope", error)
    })
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
    /// [`ReceiveOptions::wait`] is emulated rather than refused: the service answers immediately
    /// with whatever is there, so a wait re-polls at [`POLL_INTERVAL`] until a message arrives or
    /// the wait elapses. Callers therefore get what they asked for — a receive that does not
    /// report an empty queue before it has waited — without every consumer having to grow a
    /// polling loop of its own.
    async fn receive(&self, options: ReceiveOptions) -> Result<Vec<ReceivedMessage>, QueueError> {
        match options.wait {
            None => self.receive_once(options).await,
            Some(wait) => long_poll(wait, || self.receive_once(options)).await,
        }
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
        decode_message, encode_message, is_queue_name, long_poll, queue_url, receive_batch_size,
        retry_visibility_seconds, AzureStorageQueue, Duration, StorageQueueReceipt,
        MAX_MESSAGE_BYTES, MAX_RECEIVE_MESSAGES,
    };
    use serial_test::serial;
    use skyzen_services::queue::{
        envelope::BASE64_PREFIX, MessageReceipt, QueueError, QueueRetry, ReceivedMessage,
    };

    // The envelope's own round trip is tested where it lives, in
    // `skyzen_services::queue::envelope`. What is tested here is what this backend adds to it:
    // the platform's size cap, and that a malformed envelope surfaces as a `QueueError`.

    #[test]
    fn a_message_beyond_the_platform_cap_is_refused_before_it_is_sent() {
        let payload = vec![b'a'; MAX_MESSAGE_BYTES + 1];
        let error = encode_message(&payload).expect_err("an oversized message should be refused");
        assert!(error.to_string().contains("65536"), "{error}");
    }

    #[test]
    fn a_payload_that_fits_once_encoded_is_accepted() {
        let payload = vec![b'a'; MAX_MESSAGE_BYTES];
        let encoded = encode_message(&payload).expect("a payload at the cap should encode");
        assert_eq!(decode_message(&encoded).unwrap(), payload);
    }

    #[test]
    fn a_body_prefixed_base64_that_is_not_base64_is_refused() {
        let error = decode_message(&format!("{BASE64_PREFIX}not base64!"))
            .expect_err("a malformed envelope should be refused");
        assert!(error.to_string().contains("envelope"), "{error}");
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

    /// The variable this test owns. `set_var` and `remove_var` are process-wide, so the tests that
    /// touch the environment are serialized against each other.
    const SAS_URL_VARIABLE: &str = "SKYZEN_TEST_STORAGE_QUEUE_SAS_URL";

    #[test]
    #[serial]
    fn a_signed_url_read_from_the_environment_builds_a_client() {
        std::env::set_var(
            SAS_URL_VARIABLE,
            "https://skyzentest.queue.core.windows.net/jobs?sv=2024-11-04&sig=signature",
        );
        let queue = AzureStorageQueue::from_sas_env(SAS_URL_VARIABLE);
        std::env::remove_var(SAS_URL_VARIABLE);

        let queue = queue.expect("a signed URL should build a client");
        assert!(
            format!("{queue:?}").contains("skyzentest.queue.core.windows.net/jobs"),
            "{queue:?}"
        );
    }

    #[test]
    #[serial]
    fn an_unset_variable_is_reported_by_name_rather_than_as_a_missing_url() {
        std::env::remove_var(SAS_URL_VARIABLE);

        let error = AzureStorageQueue::from_sas_env(SAS_URL_VARIABLE)
            .expect_err("an unset variable should be refused");
        assert!(error.to_string().contains(SAS_URL_VARIABLE), "{error}");
    }

    #[test]
    #[serial]
    fn a_variable_holding_something_that_is_not_a_queue_url_names_the_variable() {
        std::env::set_var(SAS_URL_VARIABLE, "not a url");
        let error = AzureStorageQueue::from_sas_env(SAS_URL_VARIABLE);
        std::env::remove_var(SAS_URL_VARIABLE);

        let error = error.expect_err("a malformed URL should be refused");
        assert!(error.to_string().contains(SAS_URL_VARIABLE), "{error}");
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

    /// A message shaped like whatever the service would have returned.
    fn delivered() -> ReceivedMessage {
        ReceivedMessage {
            id: Some("1".to_owned()),
            body: b"job".to_vec(),
            receipt: MessageReceipt::new("receipt"),
            attempts: Some(1),
        }
    }

    #[tokio::test]
    async fn an_emulated_long_poll_returns_as_soon_as_a_message_arrives() {
        let polls = std::cell::Cell::new(0);

        let messages = long_poll(Duration::from_secs(30), || {
            let taken = polls.get();
            polls.set(taken + 1);
            async move {
                Ok(if taken < 2 {
                    Vec::new()
                } else {
                    vec![delivered()]
                })
            }
        })
        .await
        .expect("the poll should succeed");

        assert_eq!(messages.len(), 1);
        assert_eq!(polls.get(), 3, "it stopped as soon as it had a message");
    }

    #[tokio::test]
    async fn an_emulated_long_poll_gives_up_empty_once_the_wait_elapses() {
        let polls = std::cell::Cell::new(0);
        let started = std::time::Instant::now();
        let wait = Duration::from_millis(30);

        let messages = long_poll(wait, || {
            polls.set(polls.get() + 1);
            async { Ok(Vec::new()) }
        })
        .await
        .expect("an empty queue is not an error");

        assert_eq!(messages.len(), 0, "an elapsed wait returns nothing");
        assert!(polls.get() >= 2, "it polled again before giving up");
        assert!(started.elapsed() >= wait, "it waited out the whole poll");
    }

    #[tokio::test]
    async fn an_emulated_long_poll_surfaces_a_failure_instead_of_retrying_it() {
        let error = long_poll(Duration::from_secs(30), || async {
            Err(QueueError::backend("the queue is gone"))
        })
        .await
        .expect_err("the failure should reach the caller");

        assert!(error.to_string().contains("the queue is gone"));
    }
}
