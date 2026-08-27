//! Amazon SQS implementation of [`MessageQueue`].

use aws_sdk_sqs::operation::change_message_visibility::ChangeMessageVisibilityError;
use aws_sdk_sqs::operation::delete_message::DeleteMessageError;
use aws_sdk_sqs::operation::send_message::builders::SendMessageFluentBuilder;
use aws_sdk_sqs::types::builders::SendMessageBatchRequestEntryBuilder;
use aws_sdk_sqs::types::{
    BatchResultErrorEntry, Message, MessageAttributeValue, MessageSystemAttributeName,
    SendMessageBatchRequestEntry,
};
use aws_sdk_sqs::Client;
use aws_smithy_types::error::display::DisplayErrorContext;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use skyzen_services::queue::{
    BatchSendFailure, MessageQueue, MessageReceipt, QueueError, QueueRetry, ReceiveOptions,
    ReceivedMessage, SendOptions,
};

use crate::errors::{categorize, AwsErrorCategory};

/// The message attribute marking a base64-encoded body.
const CONTENT_ENCODING_ATTRIBUTE: &str = "skyzen-content-encoding";

/// The only value this client ever writes into [`CONTENT_ENCODING_ATTRIBUTE`].
const BASE64_ENCODING: &str = "base64";

/// How many entries SQS accepts in one `SendMessageBatch`.
const BATCH_MAX_ENTRIES: usize = 10;

/// How many messages SQS returns from one `ReceiveMessage`.
const MAX_RECEIVE_MESSAGES: i32 = 10;

/// SQS' ceiling on a per-message `DelaySeconds`, in seconds (15 minutes).
const MAX_DELAY_SECONDS: i32 = 900;

/// SQS' ceiling on a `VisibilityTimeout`, in seconds (12 hours).
const MAX_VISIBILITY_TIMEOUT_SECONDS: i32 = 43_200;

/// SQS' ceiling on a long-poll `WaitTimeSeconds`.
const MAX_WAIT_TIME_SECONDS: i32 = 20;

/// The suffix every FIFO queue URL carries.
const FIFO_SUFFIX: &str = ".fifo";

/// How a FIFO queue's `MessageDeduplicationId` is produced.
///
/// SQS deduplicates FIFO messages over a five-minute window, and it needs an id per message to do
/// it. There are exactly two places that id can come from, and picking one is a decision about the
/// *queue*, so it is configured on [`SqsQueue`] rather than passed per send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqsDeduplication {
    /// Send no id and let the queue derive one, which requires `ContentBasedDeduplication` to be
    /// enabled on the queue itself.
    ///
    /// This is the default because it is what an unconfigured FIFO queue with
    /// `ContentBasedDeduplication` set expects, and because inventing an id client-side when the
    /// queue already computes one would deduplicate twice over.
    #[default]
    Queue,
    /// Derive the id client-side from a SHA-256 digest of the message body.
    ///
    /// Use this against a queue *without* `ContentBasedDeduplication`, where SQS rejects a send
    /// that carries no id. Two identical bodies inside the deduplication window collapse into one
    /// message — the same rule the server-side option applies, computed here instead.
    ContentHash,
}

impl SqsDeduplication {
    /// The `MessageDeduplicationId` for one encoded body, if this strategy sends one.
    ///
    /// A SHA-256 digest renders as 64 hex characters, comfortably inside SQS' 128-character limit
    /// and its allowed alphabet.
    fn id_for(self, body: &str) -> Option<String> {
        match self {
            Self::Queue => None,
            Self::ContentHash => {
                use sha2::{Digest, Sha256};
                Some(format!("{:x}", Sha256::digest(body.as_bytes())))
            }
        }
    }
}

/// An Amazon SQS-backed message queue.
///
/// # Wire format
///
/// Payloads that are valid UTF-8 and contain only characters SQS allows in
/// message bodies (the W3C XML character range: `#x9`, `#xA`, `#xD`,
/// `#x20-#xD7FF`, `#xE000-#xFFFD`, `#x10000-#x10FFFF`) are sent **verbatim**,
/// so JSON produced by `send_json` arrives as plain JSON — the same wire
/// contract as the Cloudflare and in-memory backends. Any other payload is
/// base64-encoded and tagged with a `skyzen-content-encoding` message
/// attribute set to `"base64"` so consumers can detect and reverse the
/// encoding. [`receive`](MessageQueue::receive) reverses it, so a body handed to `send` comes back
/// byte-for-byte.
///
/// # Standard and FIFO queues
///
/// SQS requires a `MessageGroupId` on every send to a `.fifo` queue and rejects one on every send
/// to a standard queue, so which kind of queue this is has to be settled before the first send.
/// [`new`](SqsQueue::new) builds a standard queue and [`fifo`](SqsQueue::fifo) a FIFO one; each
/// refuses a URL of the other kind rather than letting the mismatch surface as an SDK error on the
/// first message.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Debug, Clone)]
pub struct SqsQueue {
    client: Client,
    queue_url: String,
    /// `Some` exactly when this queue is a FIFO queue; the value is sent with every message.
    message_group_id: Option<String>,
    deduplication: SqsDeduplication,
}

impl SqsQueue {
    /// Create a `SqsQueue` for a **standard** queue from an existing client and queue URL.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when `queue_url` names a FIFO queue: those need a message group id
    /// on every send, so they are built with [`fifo`](SqsQueue::fifo) instead.
    pub fn new(client: Client, queue_url: impl Into<String>) -> Result<Self, QueueError> {
        Self::configured(client, queue_url.into(), None, SqsDeduplication::default())
    }

    /// Create a `SqsQueue` for a **FIFO** queue, sending `message_group_id` with every message.
    ///
    /// Messages sharing a group id are delivered in order and one at a time; different groups
    /// proceed in parallel. Pair this with
    /// [`with_deduplication`](SqsQueue::with_deduplication) when the queue does not have
    /// `ContentBasedDeduplication` enabled.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when `queue_url` does not end in `.fifo`: SQS rejects a message
    /// group id on a standard queue, so the pairing is a configuration mistake rather than
    /// something to discover at send time.
    pub fn fifo(
        client: Client,
        queue_url: impl Into<String>,
        message_group_id: impl Into<String>,
    ) -> Result<Self, QueueError> {
        Self::configured(
            client,
            queue_url.into(),
            Some(message_group_id.into()),
            SqsDeduplication::default(),
        )
    }

    /// Create a `SqsQueue` for a standard queue from environment configuration.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when `queue_url` names a FIFO queue — see
    /// [`new`](SqsQueue::new).
    pub async fn from_env(queue_url: impl Into<String>) -> Result<Self, QueueError> {
        Self::new(Self::env_client().await, queue_url)
    }

    /// Create a `SqsQueue` for a FIFO queue from environment configuration.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when `queue_url` does not end in `.fifo` — see
    /// [`fifo`](SqsQueue::fifo).
    pub async fn fifo_from_env(
        queue_url: impl Into<String>,
        message_group_id: impl Into<String>,
    ) -> Result<Self, QueueError> {
        Self::fifo(Self::env_client().await, queue_url, message_group_id)
    }

    /// Send `message_group_id` with every message, making this a FIFO queue.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when this queue's URL does not end in `.fifo`.
    pub fn with_message_group_id(
        self,
        message_group_id: impl Into<String>,
    ) -> Result<Self, QueueError> {
        Self::configured(
            self.client,
            self.queue_url,
            Some(message_group_id.into()),
            self.deduplication,
        )
    }

    /// Choose how this queue's `MessageDeduplicationId` is produced.
    ///
    /// # Errors
    ///
    /// [`QueueError::Backend`] when this is not a FIFO queue: SQS rejects a deduplication id on a
    /// standard queue, which deduplicates nothing.
    pub fn with_deduplication(
        mut self,
        deduplication: SqsDeduplication,
    ) -> Result<Self, QueueError> {
        if self.message_group_id.is_none() && deduplication != SqsDeduplication::Queue {
            return Err(QueueError::backend(format!(
                "queue {:?} is a standard queue, which has no deduplication window; \
                 configure deduplication only on a `.fifo` queue built with `SqsQueue::fifo`",
                self.queue_url
            )));
        }
        self.deduplication = deduplication;
        Ok(self)
    }

    /// The default SDK client, built from environment configuration.
    async fn env_client() -> Client {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Client::new(&config)
    }

    /// Build a queue after checking that the FIFO configuration matches the queue URL.
    ///
    /// Both mismatches are refused here, at construction, because SQS reports either one only as a
    /// rejected `SendMessage` — after the application is already running.
    fn configured(
        client: Client,
        queue_url: String,
        message_group_id: Option<String>,
        deduplication: SqsDeduplication,
    ) -> Result<Self, QueueError> {
        match (queue_url.ends_with(FIFO_SUFFIX), message_group_id.is_some()) {
            (true, false) => Err(QueueError::backend(format!(
                "queue {queue_url:?} is a FIFO queue, and SQS rejects every send to one that \
                 carries no message group id; build it with \
                 `SqsQueue::fifo(client, url, message_group_id)`"
            ))),
            (false, true) => Err(QueueError::backend(format!(
                "queue {queue_url:?} is a standard queue, and SQS rejects every send to one that \
                 carries a message group id; build it with `SqsQueue::new(client, url)`, or point \
                 it at a `{FIFO_SUFFIX}` queue"
            ))),
            _ => Ok(Self {
                client,
                queue_url,
                message_group_id,
                deduplication,
            }),
        }
    }

    /// Whether this queue was configured as a FIFO queue.
    #[must_use]
    pub const fn is_fifo(&self) -> bool {
        self.message_group_id.is_some()
    }

    /// Apply the FIFO ordering and deduplication ids to one outgoing message.
    ///
    /// Written against the two builders' shared shape so the single-send and batch-entry paths
    /// cannot drift apart — a FIFO queue that set the group id on only one of them would reject
    /// half its traffic.
    fn apply_fifo<B>(
        &self,
        builder: B,
        body: &str,
        group: impl FnOnce(B, &str) -> B,
        dedup: impl FnOnce(B, String) -> B,
    ) -> B {
        let Some(message_group_id) = &self.message_group_id else {
            return builder;
        };
        let builder = group(builder, message_group_id);
        match self.deduplication.id_for(body) {
            Some(id) => dedup(builder, id),
            None => builder,
        }
    }
}

/// Map an AWS SDK error to a [`QueueError`], reading its service error code first.
///
/// Throttling and credential rejections become their own variants so a handler can back off or
/// give up without matching on message text; everything else keeps the full SDK message.
/// [`DisplayErrorContext`] walks the whole error source chain, so that message includes the
/// service error code instead of just "service error".
fn sdk_error<E>(err: E) -> QueueError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    match categorize(&err) {
        AwsErrorCategory::Throttled => QueueError::Throttled { retry_after: None },
        AwsErrorCategory::Unauthorized => QueueError::Unauthorized,
        AwsErrorCategory::Backend => backend_error(err),
    }
}

/// Map an error that carries no service metadata — a request builder's validation failure — to a
/// [`QueueError::Backend`] with its full source chain.
fn backend_error<E>(err: E) -> QueueError
where
    E: std::error::Error + Send + Sync + 'static,
{
    QueueError::backend_with(DisplayErrorContext(&err).to_string(), err)
}

/// Map a settle error, turning an expired or already-settled lease into [`QueueError::Conflict`].
///
/// `lease_expired` comes from the *typed* service error: `ReceiptHandleIsInvalid` and
/// `MessageNotInflight` both mean the lease this receipt named is no longer held — the visibility
/// timeout lapsed and the message went back to the queue — which is exactly the "message state
/// changed before the operation was applied" that [`QueueError::Conflict`] documents, and is not a
/// backend failure.
fn settle_error<E>(err: E, lease_expired: bool) -> QueueError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    if lease_expired {
        QueueError::Conflict
    } else {
        sdk_error(err)
    }
}

/// Whether a character is allowed in an SQS message body.
///
/// SQS documents the W3C XML character range:
/// `#x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD] | [#x10000-#x10FFFF]`.
/// (Rust `char` cannot be a surrogate, so `#xD800-#xDFFF` needs no check.)
const fn is_sqs_allowed_char(c: char) -> bool {
    matches!(c,
        '\u{9}' | '\u{A}' | '\u{D}'
        | '\u{20}'..='\u{D7FF}'
        | '\u{E000}'..='\u{FFFD}'
        | '\u{10000}'..='\u{10FFFF}')
}

/// A message body encoded for SQS transport.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EncodedMessage {
    /// The string to send as the SQS message body.
    body: String,
    /// Whether the body is base64-encoded (and must be tagged as such).
    base64: bool,
}

/// Encode a payload for SQS: verbatim when SQS accepts it as text, base64 otherwise.
fn encode_message(data: &[u8]) -> EncodedMessage {
    match core::str::from_utf8(data) {
        Ok(text) if text.chars().all(is_sqs_allowed_char) => EncodedMessage {
            body: text.to_owned(),
            base64: false,
        },
        _ => {
            use base64::Engine;
            EncodedMessage {
                body: base64::engine::general_purpose::STANDARD.encode(data),
                base64: true,
            }
        }
    }
}

/// Reverse [`encode_message`] for one received message.
///
/// An untagged body is the bytes SQS delivered. A body tagged with an encoding this client never
/// writes is refused rather than handed back wrongly decoded, because guessing would corrupt it
/// silently.
fn decode_message(message: &Message) -> Result<Vec<u8>, QueueError> {
    let body = message.body().unwrap_or_default();
    match message
        .message_attributes()
        .and_then(|attributes| attributes.get(CONTENT_ENCODING_ATTRIBUTE))
        .and_then(MessageAttributeValue::string_value)
    {
        None => Ok(body.as_bytes().to_vec()),
        Some(BASE64_ENCODING) => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(body)
                .map_err(|error| {
                    QueueError::backend_with(
                        format!(
                            "message body tagged `{CONTENT_ENCODING_ATTRIBUTE}: \
                             {BASE64_ENCODING}` is not valid base64"
                        ),
                        error,
                    )
                })
        }
        Some(other) => Err(QueueError::backend(format!(
            "message carries an unknown `{CONTENT_ENCODING_ATTRIBUTE}` of {other:?}; \
             this client only writes {BASE64_ENCODING:?}"
        ))),
    }
}

/// The message attribute value tagging a base64-encoded body.
fn base64_attribute() -> Result<MessageAttributeValue, QueueError> {
    MessageAttributeValue::builder()
        .data_type("String")
        .string_value(BASE64_ENCODING)
        .build()
        .map_err(backend_error)
}

/// The batch entry id for the message at `index` of the slice `send_batch` was given.
///
/// Ids only have to be unique inside one `SendMessageBatch` request, but making them unique across
/// the whole call is what lets [`batch_send_failure`] recover the caller's index from a failure
/// SQS reports — a per-chunk `0..10` id would name the wrong message from the second chunk on.
const fn batch_entry_id(chunk_index: usize, offset: usize) -> usize {
    chunk_index * BATCH_MAX_ENTRIES + offset
}

/// Convert one SQS per-entry failure into the portable [`BatchSendFailure`].
///
/// # Errors
///
/// [`QueueError::Backend`] when the entry id is not one [`batch_entry_id`] produced, rather than
/// reporting a fabricated index that would point at the wrong message.
fn batch_send_failure(entry: &BatchResultErrorEntry) -> Result<BatchSendFailure, QueueError> {
    let index = entry.id().parse::<usize>().map_err(|error| {
        QueueError::backend_with(
            format!(
                "SQS reported a failure for batch entry id {:?}, which this client did not send",
                entry.id()
            ),
            error,
        )
    })?;

    Ok(BatchSendFailure {
        index,
        code: entry.code().to_owned(),
        message: entry.message().unwrap_or_default().to_owned(),
    })
}

/// Render a duration as whole seconds for an SQS parameter, rounding sub-second parts up.
///
/// `parameter` and `maximum` name the platform limit in the error, so a caller who asks for more
/// than SQS accepts learns which knob and which ceiling instead of getting a validation failure
/// back off the wire.
fn duration_seconds(
    duration: core::time::Duration,
    parameter: &str,
    maximum: i32,
) -> Result<i32, QueueError> {
    let mut seconds = duration.as_secs();
    if duration.subsec_nanos() > 0 {
        seconds = seconds.saturating_add(1);
    }

    i32::try_from(seconds)
        .ok()
        .filter(|seconds| *seconds <= maximum)
        .ok_or_else(|| {
            QueueError::backend(format!(
                "SQS caps {parameter} at {maximum} seconds; {seconds} was requested"
            ))
        })
}

/// SQS' `MaxNumberOfMessages` for a requested batch size.
///
/// One `ReceiveMessage` returns at most ten messages, so a larger request is **capped** rather
/// than refused: a consumer that wants more polls again, and the alternative — chunking into
/// several calls — would lease messages the caller could not settle if a later call failed. Zero
/// is refused, because an empty result would be indistinguishable from an empty queue.
fn receive_batch_size(max_messages: usize) -> Result<i32, QueueError> {
    if max_messages == 0 {
        return Err(QueueError::backend(
            "ReceiveOptions::max_messages must be at least 1; asking SQS for zero messages \
             would look the same as an empty queue",
        ));
    }

    Ok(
        i32::try_from(max_messages).map_or(MAX_RECEIVE_MESSAGES, |requested| {
            requested.min(MAX_RECEIVE_MESSAGES)
        }),
    )
}

/// The `VisibilityTimeout` that carries out a [`QueueRetry`].
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
                "SQS caps a redelivery delay at {MAX_VISIBILITY_TIMEOUT_SECONDS} seconds; \
                 {delay_seconds} was requested"
            ))
        })
}

/// Read `ApproximateReceiveCount` off a received message.
///
/// # Errors
///
/// [`QueueError::Backend`] when the attribute is present but is not a count, which would mean the
/// delivery counter this reports cannot be trusted.
fn receive_count(message: &Message) -> Result<Option<u32>, QueueError> {
    message
        .attributes()
        .and_then(|attributes| attributes.get(&MessageSystemAttributeName::ApproximateReceiveCount))
        .map(|count| {
            count.parse::<u32>().map_err(|error| {
                QueueError::backend_with(
                    format!("SQS reported an ApproximateReceiveCount of {count:?}, not a count"),
                    error,
                )
            })
        })
        .transpose()
}

/// Turn one delivered SQS message into a leased [`ReceivedMessage`].
fn received_message(message: &Message) -> Result<ReceivedMessage, QueueError> {
    let Some(receipt_handle) = message.receipt_handle() else {
        return Err(QueueError::backend(
            "SQS delivered a message with no receipt handle, which can never be acknowledged",
        ));
    };

    Ok(ReceivedMessage {
        id: message.message_id().map(ToOwned::to_owned),
        body: decode_message(message)?,
        receipt: MessageReceipt::new(receipt_handle),
        attempts: receive_count(message)?,
    })
}

impl MessageQueue for SqsQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        self.send_with(message, SendOptions::new()).await
    }

    async fn send_with(&self, message: &[u8], options: SendOptions) -> Result<(), QueueError> {
        let encoded = encode_message(message);

        let mut request = self
            .client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body(encoded.body.as_str());

        if encoded.base64 {
            request = request.message_attributes(CONTENT_ENCODING_ATTRIBUTE, base64_attribute()?);
        }

        if let Some(delay) = options.delay {
            // A FIFO queue takes its delay on the queue, not per message: SQS rejects
            // `DelaySeconds` on a `.fifo` send outright.
            if self.is_fifo() {
                return Err(QueueError::Unsupported(
                    "SQS rejects a per-message delay on a FIFO queue; set the queue's own \
                     delivery delay instead",
                ));
            }
            request = request.delay_seconds(duration_seconds(
                delay,
                "a message delay",
                MAX_DELAY_SECONDS,
            )?);
        }

        request = self.apply_fifo(
            request,
            &encoded.body,
            |request, group| request.message_group_id(group),
            SendMessageFluentBuilder::message_deduplication_id,
        );

        request.send().await.map_err(sdk_error)?;
        Ok(())
    }

    /// Send a batch of messages in chunks of ten (the SQS batch maximum).
    ///
    /// Delivery is at-least-once and **not atomic**: entries SQS accepts are delivered even when
    /// others in the same request are rejected, and earlier chunks are already delivered when a
    /// later one fails. Rejected entries are collected across every chunk and reported together as
    /// [`QueueError::PartialBatch`], each carrying its index in `messages` — so retrying exactly
    /// the failures is a matter of re-sending those elements.
    ///
    /// A chunk that fails as a whole (a throttle, a credential rejection, a transport error) stops
    /// the send there and surfaces as that error rather than as a `PartialBatch`. Chunks already
    /// accepted stay delivered, and per-entry failures collected before it cannot ride along on an
    /// error of a different shape — so a caller recovering from one has to treat the whole batch as
    /// being of unknown state, which at-least-once delivery already allows for.
    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        let mut failures = Vec::new();

        for (chunk_index, chunk) in messages.chunks(BATCH_MAX_ENTRIES).enumerate() {
            let mut batch = self.client.send_message_batch().queue_url(&self.queue_url);

            for (offset, message) in chunk.iter().enumerate() {
                let encoded = encode_message(message);
                let mut entry = SendMessageBatchRequestEntry::builder()
                    .id(batch_entry_id(chunk_index, offset).to_string())
                    .message_body(encoded.body.as_str());

                if encoded.base64 {
                    entry =
                        entry.message_attributes(CONTENT_ENCODING_ATTRIBUTE, base64_attribute()?);
                }

                entry = self.apply_fifo(
                    entry,
                    &encoded.body,
                    |entry, group| entry.message_group_id(group),
                    SendMessageBatchRequestEntryBuilder::message_deduplication_id,
                );

                batch = batch.entries(entry.build().map_err(backend_error)?);
            }

            let result = batch.send().await.map_err(sdk_error)?;
            for entry in &result.failed {
                failures.push(batch_send_failure(entry)?);
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(QueueError::PartialBatch { failures })
        }
    }

    /// Take up to ten messages off the queue, leasing each one.
    ///
    /// [`ReceiveOptions::max_messages`] above ten is capped at ten rather than split across
    /// several requests, [`ReceiveOptions::wait`] becomes SQS' long-poll `WaitTimeSeconds`, and
    /// [`ReceiveOptions::visibility_timeout`] the lease length. Bodies are decoded back through
    /// the wire format this backend sends, so what `send` was handed is what `receive` returns.
    async fn receive(&self, options: ReceiveOptions) -> Result<Vec<ReceivedMessage>, QueueError> {
        let mut request = self
            .client
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(receive_batch_size(options.max_messages)?)
            // Without asking for it by name SQS returns no message attributes at all, and the
            // base64 tag would be invisible — bodies would come back still encoded.
            .message_attribute_names(CONTENT_ENCODING_ATTRIBUTE)
            .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount);

        if let Some(visibility_timeout) = options.visibility_timeout {
            request = request.visibility_timeout(duration_seconds(
                visibility_timeout,
                "a visibility timeout",
                MAX_VISIBILITY_TIMEOUT_SECONDS,
            )?);
        }

        if let Some(wait) = options.wait {
            request = request.wait_time_seconds(duration_seconds(
                wait,
                "a long-poll wait",
                MAX_WAIT_TIME_SECONDS,
            )?);
        }

        let output = request.send().await.map_err(sdk_error)?;
        output.messages().iter().map(received_message).collect()
    }

    async fn ack(&self, receipt: &MessageReceipt) -> Result<(), QueueError> {
        self.client
            .delete_message()
            .queue_url(&self.queue_url)
            .receipt_handle(receipt.as_str())
            .send()
            .await
            .map_err(|err| {
                let lease_expired = err
                    .as_service_error()
                    .is_some_and(DeleteMessageError::is_receipt_handle_is_invalid);
                settle_error(err, lease_expired)
            })?;
        Ok(())
    }

    /// Return a message to the queue by clearing its visibility timeout.
    ///
    /// [`QueueRetry::delay_seconds`] becomes the new timeout, so `None` redelivers as soon as SQS
    /// can and a delay holds the message invisible for exactly that long first.
    async fn nack(&self, receipt: &MessageReceipt, retry: QueueRetry) -> Result<(), QueueError> {
        self.client
            .change_message_visibility()
            .queue_url(&self.queue_url)
            .receipt_handle(receipt.as_str())
            .visibility_timeout(retry_visibility_seconds(retry)?)
            .send()
            .await
            .map_err(|err| {
                let lease_expired =
                    err.as_service_error()
                        .is_some_and(|error: &ChangeMessageVisibilityError| {
                            error.is_receipt_handle_is_invalid() || error.is_message_not_inflight()
                        });
                settle_error(err, lease_expired)
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        batch_entry_id, batch_send_failure, decode_message, duration_seconds, encode_message,
        is_sqs_allowed_char, receive_batch_size, receive_count, received_message,
        retry_visibility_seconds, BatchResultErrorEntry, Message, MessageAttributeValue,
        MessageSystemAttributeName, SqsDeduplication, SqsQueue, MAX_DELAY_SECONDS,
        MAX_VISIBILITY_TIMEOUT_SECONDS, MAX_WAIT_TIME_SECONDS,
    };
    use aws_sdk_sqs::config::{BehaviorVersion, Credentials, Region};
    use aws_sdk_sqs::{Client, Config};
    use core::time::Duration;
    use skyzen_services::queue::{QueueError, QueueRetry};

    const STANDARD_URL: &str = "https://sqs.us-east-1.amazonaws.com/123456789012/jobs";
    const FIFO_URL: &str = "https://sqs.us-east-1.amazonaws.com/123456789012/jobs.fifo";

    /// A client with fixed credentials; every test here stops before any request is issued.
    fn client() -> Client {
        Client::from_conf(
            Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("us-east-1"))
                .credentials_provider(Credentials::new("AKIDTEST", "secret", None, None, "tests"))
                .build(),
        )
    }

    #[test]
    fn json_passes_through_unchanged() {
        let payload = br#"{"kind":"email","to":"user@skyzen.rs"}"#;
        let encoded = encode_message(payload);
        assert!(!encoded.base64);
        assert_eq!(encoded.body.as_bytes(), payload);
    }

    #[test]
    fn plain_text_with_unicode_passes_through() {
        let payload = "hello 世界 \t\r\n".as_bytes();
        let encoded = encode_message(payload);
        assert!(!encoded.base64);
        assert_eq!(encoded.body.as_bytes(), payload);
    }

    #[test]
    fn invalid_utf8_is_base64_encoded() {
        use base64::Engine;

        let payload = [0xFF, 0xFE, 0x00, 0x01];
        let encoded = encode_message(&payload);
        assert!(encoded.base64);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded.body)
            .unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn utf8_with_disallowed_control_chars_is_base64_encoded() {
        let payload = b"null byte \x00 inside";
        let encoded = encode_message(payload);
        assert!(encoded.base64);
    }

    #[test]
    fn allowed_char_boundaries() {
        assert!(is_sqs_allowed_char('\u{9}'));
        assert!(is_sqs_allowed_char('\u{A}'));
        assert!(is_sqs_allowed_char('\u{D}'));
        assert!(is_sqs_allowed_char(' '));
        assert!(is_sqs_allowed_char('\u{D7FF}'));
        assert!(is_sqs_allowed_char('\u{E000}'));
        assert!(is_sqs_allowed_char('\u{FFFD}'));
        assert!(is_sqs_allowed_char('\u{10000}'));
        assert!(is_sqs_allowed_char('\u{10FFFF}'));
        assert!(!is_sqs_allowed_char('\u{0}'));
        assert!(!is_sqs_allowed_char('\u{B}'));
        assert!(!is_sqs_allowed_char('\u{1F}'));
        assert!(!is_sqs_allowed_char('\u{FFFE}'));
    }

    /// Build the message SQS would deliver for a payload this backend sent.
    fn delivered(payload: &[u8], receive_count: Option<&str>) -> Message {
        let encoded = encode_message(payload);
        let mut message = Message::builder()
            .message_id("m-1")
            .receipt_handle("lease-1")
            .body(&encoded.body);

        if encoded.base64 {
            message = message.message_attributes(
                super::CONTENT_ENCODING_ATTRIBUTE,
                MessageAttributeValue::builder()
                    .data_type("String")
                    .string_value(super::BASE64_ENCODING)
                    .build()
                    .unwrap(),
            );
        }

        if let Some(count) = receive_count {
            message = message.attributes(
                MessageSystemAttributeName::ApproximateReceiveCount,
                count.to_owned(),
            );
        }

        message.build()
    }

    #[test]
    fn receive_reverses_the_send_side_encoding_for_text_and_binary() {
        for payload in [
            br#"{"kind":"email"}"#.as_slice(),
            "héllo 世界".as_bytes(),
            &[0xFF, 0xFE, 0x00, 0x01],
            b"null byte \x00 inside",
            b"",
        ] {
            let received = received_message(&delivered(payload, None)).unwrap();
            assert_eq!(received.body, payload, "round trip lost {payload:?}");
            assert_eq!(received.receipt.as_str(), "lease-1");
            assert_eq!(received.id.as_deref(), Some("m-1"));
        }
    }

    #[test]
    fn receive_reads_the_delivery_count_and_refuses_a_malformed_one() {
        let received = received_message(&delivered(b"job", Some("3"))).unwrap();
        assert_eq!(received.attempts, Some(3));

        assert!(received_message(&delivered(b"job", None))
            .unwrap()
            .attempts
            .is_none());

        let malformed = delivered(b"job", Some("many"));
        assert!(receive_count(&malformed).is_err());
    }

    #[test]
    fn an_unknown_content_encoding_tag_is_refused_rather_than_guessed() {
        let message = Message::builder()
            .receipt_handle("lease-1")
            .body("cGF5bG9hZA==")
            .message_attributes(
                super::CONTENT_ENCODING_ATTRIBUTE,
                MessageAttributeValue::builder()
                    .data_type("String")
                    .string_value("gzip")
                    .build()
                    .unwrap(),
            )
            .build();

        let error = decode_message(&message).unwrap_err();
        assert!(error.to_string().contains("unknown"));
    }

    #[test]
    fn a_message_without_a_receipt_handle_is_refused() {
        let message = Message::builder().body("job").build();
        let error = received_message(&message).unwrap_err();
        assert!(error.to_string().contains("receipt handle"));
    }

    #[test]
    fn batch_entry_ids_are_the_index_in_the_callers_slice() {
        assert_eq!(batch_entry_id(0, 0), 0);
        assert_eq!(batch_entry_id(0, 9), 9);
        assert_eq!(batch_entry_id(1, 0), 10);
        assert_eq!(batch_entry_id(2, 7), 27);

        // Every id across a 25-message send is distinct and covers exactly 0..25.
        let ids: Vec<usize> = (0..25)
            .map(|index| batch_entry_id(index / 10, index % 10))
            .collect();
        assert_eq!(ids, (0..25).collect::<Vec<_>>());
    }

    #[test]
    fn a_batch_failure_reports_the_index_in_the_callers_slice() {
        // Entry 27 lives in the third chunk; a per-chunk id would have named message 7 instead.
        let entry = BatchResultErrorEntry::builder()
            .id(batch_entry_id(2, 7).to_string())
            .code("InternalError")
            .message("something broke")
            .sender_fault(false)
            .build()
            .unwrap();

        let failure = batch_send_failure(&entry).unwrap();
        assert_eq!(failure.index, 27);
        assert_eq!(failure.code, "InternalError");
        assert_eq!(failure.message, "something broke");
    }

    #[test]
    fn a_batch_failure_without_a_message_keeps_its_code() {
        let entry = BatchResultErrorEntry::builder()
            .id("4")
            .code("InvalidMessageContents")
            .sender_fault(true)
            .build()
            .unwrap();

        let failure = batch_send_failure(&entry).unwrap();
        assert_eq!(failure.index, 4);
        assert!(failure.message.is_empty(), "{:?}", failure.message);
    }

    #[test]
    fn a_batch_failure_for_an_id_this_client_never_sent_is_refused() {
        let entry = BatchResultErrorEntry::builder()
            .id("msg-abc")
            .code("InternalError")
            .sender_fault(false)
            .build()
            .unwrap();

        assert!(batch_send_failure(&entry).is_err());
    }

    #[test]
    fn a_fifo_url_without_a_message_group_id_is_refused_at_construction() {
        let error = SqsQueue::new(client(), FIFO_URL).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("FIFO"), "{message}");
        assert!(message.contains("SqsQueue::fifo"), "{message}");
    }

    #[test]
    fn a_standard_url_with_a_message_group_id_is_refused_at_construction() {
        let error = SqsQueue::fifo(client(), STANDARD_URL, "orders").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("standard queue"), "{message}");
        assert!(message.contains("SqsQueue::new"), "{message}");

        let error = SqsQueue::new(client(), STANDARD_URL)
            .unwrap()
            .with_message_group_id("orders")
            .unwrap_err();
        assert!(error.to_string().contains("standard queue"));
    }

    #[test]
    fn a_matching_url_and_configuration_is_accepted() {
        assert!(!SqsQueue::new(client(), STANDARD_URL).unwrap().is_fifo());
        assert!(SqsQueue::fifo(client(), FIFO_URL, "orders")
            .unwrap()
            .is_fifo());
        assert!(SqsQueue::fifo(client(), FIFO_URL, "orders")
            .unwrap()
            .with_deduplication(SqsDeduplication::ContentHash)
            .is_ok());
    }

    #[test]
    fn deduplication_cannot_be_configured_on_a_standard_queue() {
        let error = SqsQueue::new(client(), STANDARD_URL)
            .unwrap()
            .with_deduplication(SqsDeduplication::ContentHash)
            .unwrap_err();
        assert!(error.to_string().contains("standard queue"));
    }

    #[test]
    fn content_hash_deduplication_is_stable_per_body_and_fits_the_sqs_id_limit() {
        let first = SqsDeduplication::ContentHash.id_for("payload").unwrap();
        let again = SqsDeduplication::ContentHash.id_for("payload").unwrap();
        let other = SqsDeduplication::ContentHash.id_for("payload2").unwrap();

        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));

        assert!(SqsDeduplication::Queue.id_for("payload").is_none());
    }

    #[test]
    fn durations_round_up_to_whole_seconds_and_respect_each_platform_cap() {
        assert_eq!(
            duration_seconds(
                Duration::from_millis(1500),
                "a message delay",
                MAX_DELAY_SECONDS
            )
            .unwrap(),
            2
        );
        assert_eq!(
            duration_seconds(Duration::ZERO, "a message delay", MAX_DELAY_SECONDS).unwrap(),
            0
        );
        assert_eq!(
            duration_seconds(
                Duration::from_mins(15),
                "a message delay",
                MAX_DELAY_SECONDS
            )
            .unwrap(),
            900
        );

        let error = duration_seconds(
            Duration::from_secs(901),
            "a message delay",
            MAX_DELAY_SECONDS,
        )
        .unwrap_err();
        assert!(error.to_string().contains("900"), "{error}");

        assert!(duration_seconds(
            Duration::from_secs(21),
            "a long-poll wait",
            MAX_WAIT_TIME_SECONDS
        )
        .is_err());
        assert!(duration_seconds(
            Duration::from_secs(43_201),
            "a visibility timeout",
            MAX_VISIBILITY_TIMEOUT_SECONDS
        )
        .is_err());
        assert!(duration_seconds(Duration::MAX, "a message delay", MAX_DELAY_SECONDS).is_err());
    }

    #[test]
    fn receive_batch_size_caps_at_ten_and_refuses_zero() {
        assert_eq!(receive_batch_size(1).unwrap(), 1);
        assert_eq!(receive_batch_size(10).unwrap(), 10);
        assert_eq!(receive_batch_size(500).unwrap(), 10);
        assert_eq!(receive_batch_size(usize::MAX).unwrap(), 10);
        assert!(receive_batch_size(0).is_err());
    }

    #[test]
    fn a_retry_without_a_delay_makes_the_message_visible_immediately() {
        assert_eq!(retry_visibility_seconds(QueueRetry::new()).unwrap(), 0);
        assert_eq!(
            retry_visibility_seconds(QueueRetry::new().with_delay_seconds(30)).unwrap(),
            30
        );
        assert!(retry_visibility_seconds(QueueRetry::new().with_delay_seconds(u32::MAX)).is_err());
    }

    #[tokio::test]
    async fn a_fifo_queue_refuses_a_per_message_delay() {
        use skyzen_services::queue::{MessageQueue, SendOptions};

        let queue = SqsQueue::fifo(client(), FIFO_URL, "orders").unwrap();
        let error = queue
            .send_with(
                b"job",
                SendOptions::new().with_delay(Duration::from_secs(30)),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, QueueError::Unsupported(_)), "{error}");
        assert!(error.to_string().contains("FIFO"));
    }
}
