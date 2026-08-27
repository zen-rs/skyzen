//! Turning an SQS invocation into a portable [`QueueBatch`] and back into a batch response.

use aws_lambda_events::sqs::{SqsBatchResponse, SqsEvent, SqsMessage};
use base64::Engine as _;
use skyzen_services::queue::{
    QueueBatch, QueueBatchDisposition, QueueMessage, QueueMessageDisposition,
};

/// The message attribute [`skyzen_aws::SqsQueue`] tags a base64-encoded body with.
///
/// The wire format is owned by `aws/src/sqs.rs`; this reverses it for a message the platform
/// pushed rather than one the client received.
const CONTENT_ENCODING_ATTRIBUTE: &str = "skyzen-content-encoding";

/// The only encoding that attribute is ever set to.
const BASE64_ENCODING: &str = "base64";

/// The system attribute carrying how many times a message has been delivered.
const RECEIVE_COUNT_ATTRIBUTE: &str = "ApproximateReceiveCount";

/// The system attribute carrying when the message was sent, in epoch milliseconds.
const SENT_TIMESTAMP_ATTRIBUTE: &str = "SentTimestamp";

/// The `eventSource` every SQS record carries.
pub const SQS_EVENT_SOURCE: &str = "aws:sqs";

/// A batch that could not be turned into something the handler can be given.
///
/// Every variant is a malformed event rather than a handler failure: retrying would produce the
/// same event, so the invocation fails outright instead of reporting per-message failures.
#[derive(Debug)]
pub enum DecodeError {
    /// A record arrived with no `messageId`, so its outcome could never be reported back.
    MissingMessageId,
    /// A body is tagged with an encoding this framework never writes.
    UnknownEncoding(String),
    /// A body is tagged as base64 but is not base64.
    MalformedBase64(base64::DecodeError),
    /// `ApproximateReceiveCount` is present but is not a count.
    MalformedReceiveCount(String),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingMessageId => write!(
                f,
                "SQS delivered a record with no messageId, which can never be reported as failed"
            ),
            Self::UnknownEncoding(encoding) => write!(
                f,
                "a message carries an unknown `{CONTENT_ENCODING_ATTRIBUTE}` of {encoding:?}; \
                 Skyzen only writes {BASE64_ENCODING:?}"
            ),
            Self::MalformedBase64(error) => write!(
                f,
                "a message body tagged `{CONTENT_ENCODING_ATTRIBUTE}: {BASE64_ENCODING}` is not \
                 valid base64: {error}"
            ),
            Self::MalformedReceiveCount(count) => write!(
                f,
                "SQS reported an {RECEIVE_COUNT_ATTRIBUTE} of {count:?}, not a count"
            ),
        }
    }
}

impl core::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::MalformedBase64(error) => Some(error),
            _ => None,
        }
    }
}

/// One delivered SQS batch, decoded into what the handler sees and what the response needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedBatch {
    /// The batch handed to the `#[skyzen::queue]` handler.
    pub batch: QueueBatch<Vec<u8>>,
    /// The `messageId` of each message, positionally matching [`Self::batch`], for the batch
    /// response. Kept beside the batch rather than read back off it, because a portable
    /// [`QueueMessage`] is free to carry any id it likes.
    pub message_ids: Vec<String>,
    /// The highest delivery count in the batch, for the log span.
    pub attempts: Option<u32>,
}

/// Decode an SQS invocation into a portable batch.
///
/// [`QueueBatch::queue`] is the queue's name as taken from the record's `eventSourceARN`: an
/// invocation carries no portable `[[service]]` name, and the ARN's last segment is the queue.
///
/// # Errors
///
/// [`DecodeError`] when a record is missing its id, carries an encoding tag Skyzen never writes,
/// or reports a delivery count that is not a number.
pub fn decode(event: SqsEvent) -> Result<DecodedBatch, DecodeError> {
    let queue = event
        .records
        .first()
        .and_then(|record| record.event_source_arn.as_deref())
        .map_or_else(String::new, queue_name_of);

    let mut messages = Vec::with_capacity(event.records.len());
    let mut message_ids = Vec::with_capacity(event.records.len());
    let mut attempts = None;

    // Consumed rather than borrowed: an untagged body is already the bytes the handler wants, so
    // moving the `String` out of the record spares a copy of every message in the batch.
    for record in event.records {
        let id = record
            .message_id
            .clone()
            .ok_or(DecodeError::MissingMessageId)?;
        attempts = attempts.max(receive_count(&record)?);
        let timestamp_ms = sent_timestamp_ms(&record);
        messages.push(QueueMessage {
            id: id.clone(),
            timestamp_ms,
            body: decode_body(record)?,
        });
        message_ids.push(id);
    }

    Ok(DecodedBatch {
        batch: QueueBatch { queue, messages },
        message_ids,
        attempts,
    })
}

/// The queue's name, which is the last segment of its ARN.
fn queue_name_of(arn: &str) -> String {
    arn.rsplit(':').next().unwrap_or(arn).to_owned()
}

/// Reverse the encoding `SqsQueue::send` applied, if any.
fn decode_body(record: SqsMessage) -> Result<Vec<u8>, DecodeError> {
    let encoding = record
        .message_attributes
        .get(CONTENT_ENCODING_ATTRIBUTE)
        .and_then(|attribute| attribute.string_value.as_deref())
        .map(ToOwned::to_owned);
    let body = record.body.unwrap_or_default();

    match encoding.as_deref() {
        None => Ok(body.into_bytes()),
        Some(BASE64_ENCODING) => base64::engine::general_purpose::STANDARD
            .decode(&body)
            .map_err(DecodeError::MalformedBase64),
        Some(other) => Err(DecodeError::UnknownEncoding(other.to_owned())),
    }
}

/// How many times this message has been delivered, when SQS says.
fn receive_count(record: &SqsMessage) -> Result<Option<u32>, DecodeError> {
    record
        .attributes
        .get(RECEIVE_COUNT_ATTRIBUTE)
        .map(|count| {
            count
                .parse::<u32>()
                .map_err(|_| DecodeError::MalformedReceiveCount(count.clone()))
        })
        .transpose()
}

/// When the message was enqueued, or zero when SQS did not say.
///
/// Unlike the polling driver — which can only report when it *received* a batch — a pushed
/// invocation carries `SentTimestamp`, so this is the enqueue time Cloudflare's `timestamp_ms`
/// also reports.
fn sent_timestamp_ms(record: &SqsMessage) -> i64 {
    record
        .attributes
        .get(SENT_TIMESTAMP_ATTRIBUTE)
        .and_then(|sent| sent.parse::<i64>().ok())
        .unwrap_or_default()
}

/// Report which messages must be redelivered.
///
/// Lambda deletes the messages a partial batch response does *not* name, so anything other than a
/// plain acknowledgement has to list ids. A per-message decision list that does not line up with
/// the batch is refused the same way the polling driver refuses it: every message is retried,
/// because settling by index against a mismatched list would delete whichever messages happened to
/// line up.
///
/// This shape only takes effect when the event source mapping enables `ReportBatchItemFailures`;
/// without it Lambda retries the whole batch on any failure.
///
/// A [`QueueRetry`](skyzen_services::queue::QueueRetry) delay the handler asks for has nowhere to
/// go here: when a message becomes visible again is the event source mapping's decision, not the
/// function's, so only the retry *itself* crosses over.
#[must_use]
pub fn batch_response(
    disposition: &QueueBatchDisposition,
    message_ids: &[String],
) -> SqsBatchResponse {
    let failed: Vec<&String> = match disposition {
        QueueBatchDisposition::All(QueueMessageDisposition::Ack) => Vec::new(),
        QueueBatchDisposition::All(QueueMessageDisposition::Retry(_)) => {
            message_ids.iter().collect()
        }
        QueueBatchDisposition::PerMessage(decisions) if decisions.len() == message_ids.len() => {
            decisions
                .iter()
                .zip(message_ids)
                .filter(|(decision, _)| matches!(decision, QueueMessageDisposition::Retry(_)))
                .map(|(_, id)| id)
                .collect()
        }
        QueueBatchDisposition::PerMessage(decisions) => {
            tracing::error!(
                decisions = decisions.len(),
                messages = message_ids.len(),
                "queue handler returned one decision per message for a different batch size; \
                 retrying the batch"
            );
            message_ids.iter().collect()
        }
    };

    retry_all(failed)
}

/// Report every message as failed, which is how a handler error and a panic are both settled.
#[must_use]
pub fn retry_all<'a>(message_ids: impl IntoIterator<Item = &'a String>) -> SqsBatchResponse {
    let mut response = SqsBatchResponse::default();
    for id in message_ids {
        response.add_failure(id.clone());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::{batch_response, decode, retry_all, DecodeError, SQS_EVENT_SOURCE};
    use aws_lambda_events::sqs::SqsEvent;
    use skyzen_services::queue::{QueueBatchDisposition, QueueMessageDisposition, QueueRetry};

    /// Two plain messages, one of them redelivered.
    const PLAIN: &str = include_str!("../tests/fixtures/sqs-plain.json");
    /// One base64-tagged message, as `SqsQueue::send` writes a binary payload.
    const BASE64: &str = include_str!("../tests/fixtures/sqs-base64.json");
    /// A message tagged with an encoding Skyzen never writes.
    const UNKNOWN_ENCODING: &str = include_str!("../tests/fixtures/sqs-unknown-encoding.json");

    fn event(fixture: &str) -> SqsEvent {
        serde_json::from_str(fixture).expect("fixture is a valid SQS event")
    }

    #[test]
    fn decodes_plain_bodies_with_the_queue_name_and_delivery_count() {
        let decoded = decode(event(PLAIN)).expect("the fixture decodes");

        assert_eq!(decoded.batch.queue, "skyzen-jobs");
        assert_eq!(decoded.batch.messages.len(), 2);
        assert_eq!(decoded.batch.messages[0].body, b"{\"job\":\"first\"}");
        assert_eq!(decoded.batch.messages[1].body, b"second");
        // The enqueue time, not the moment the batch arrived.
        assert_eq!(decoded.batch.messages[0].timestamp_ms, 1_545_082_649_183);
        assert_eq!(
            decoded.message_ids,
            vec![
                "059f36b4-87a3-44ab-83d2-661975830a7d".to_owned(),
                "2e1424d4-f796-459a-8184-9c92662be6da".to_owned(),
            ]
        );
        // The highest count in the batch, so one redelivered message is visible.
        assert_eq!(decoded.attempts, Some(3));
    }

    #[test]
    fn reverses_the_base64_encoding_the_sqs_client_writes() {
        let decoded = decode(event(BASE64)).expect("the fixture decodes");

        assert_eq!(decoded.batch.messages[0].body, vec![0x00, 0x01, 0xff]);
    }

    #[test]
    fn refuses_an_encoding_it_never_writes_rather_than_guessing() {
        let error = decode(event(UNKNOWN_ENCODING)).expect_err("unknown encoding");

        assert!(matches!(error, DecodeError::UnknownEncoding(_)));
        assert!(error.to_string().contains("gzip"), "{error}");
    }

    #[test]
    fn every_record_in_the_fixtures_is_an_sqs_record() {
        for fixture in [PLAIN, BASE64, UNKNOWN_ENCODING] {
            let event = event(fixture);
            assert!(event
                .records
                .iter()
                .all(|record| record.event_source.as_deref() == Some(SQS_EVENT_SOURCE)));
        }
    }

    #[test]
    fn an_acknowledged_batch_reports_no_failures() {
        let ids = vec!["a".to_owned(), "b".to_owned()];
        let response = batch_response(&QueueBatchDisposition::ack_all(), &ids);

        assert!(response.batch_item_failures.is_empty());
    }

    #[test]
    fn a_retried_batch_names_every_message() {
        let ids = vec!["a".to_owned(), "b".to_owned()];
        let response = batch_response(&QueueBatchDisposition::retry_all(QueueRetry::new()), &ids);

        assert_eq!(
            response
                .batch_item_failures
                .iter()
                .map(|failure| failure.item_identifier.clone())
                .collect::<Vec<_>>(),
            ids
        );
    }

    #[test]
    fn a_per_message_decision_names_only_the_failures() {
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let response = batch_response(
            &QueueBatchDisposition::PerMessage(vec![
                QueueMessageDisposition::Ack,
                QueueMessageDisposition::Retry(QueueRetry::new()),
                QueueMessageDisposition::Ack,
            ]),
            &ids,
        );

        assert_eq!(response.batch_item_failures.len(), 1);
        assert_eq!(response.batch_item_failures[0].item_identifier, "b");
    }

    #[test]
    fn a_mismatched_decision_list_retries_everything_rather_than_settling_by_index() {
        let ids = vec!["a".to_owned(), "b".to_owned()];
        let response = batch_response(
            &QueueBatchDisposition::PerMessage(vec![QueueMessageDisposition::Ack]),
            &ids,
        );

        assert_eq!(response.batch_item_failures.len(), 2);
    }

    #[test]
    fn retry_all_names_every_message_given() {
        let ids = vec!["a".to_owned(), "b".to_owned()];
        let response = retry_all(&ids);

        assert_eq!(response.batch_item_failures.len(), 2);
        assert_eq!(response.batch_item_failures[1].item_identifier, "b");
    }
}
