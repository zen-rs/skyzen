//! Amazon SQS implementation of [`MessageQueue`].

use aws_sdk_sqs::types::{BatchResultErrorEntry, MessageAttributeValue};
use aws_sdk_sqs::Client;
use aws_smithy_types::error::display::DisplayErrorContext;
use skyzen_services::queue::{MessageQueue, QueueError};

/// The message attribute marking a base64-encoded body.
const CONTENT_ENCODING_ATTRIBUTE: &str = "skyzen-content-encoding";

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
/// encoding.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Debug, Clone)]
pub struct SqsQueue {
    client: Client,
    queue_url: String,
}

impl SqsQueue {
    /// Create a new `SqsQueue` from an existing client and queue URL.
    pub fn new(client: Client, queue_url: impl Into<String>) -> Self {
        Self {
            client,
            queue_url: queue_url.into(),
        }
    }

    /// Create a new `SqsQueue` from environment configuration.
    pub async fn from_env(queue_url: impl Into<String>) -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = Client::new(&config);
        Self::new(client, queue_url)
    }
}

/// Map an AWS SDK error to a [`QueueError::Backend`] with full error context.
///
/// [`DisplayErrorContext`] walks the whole error source chain, so the message
/// includes the service error code and message instead of just "service error".
fn backend_error<E>(err: E) -> QueueError
where
    E: std::error::Error + Send + Sync + 'static,
{
    QueueError::backend_with(DisplayErrorContext(&err).to_string(), err)
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

/// The message attribute value tagging a base64-encoded body.
fn base64_attribute() -> Result<MessageAttributeValue, QueueError> {
    MessageAttributeValue::builder()
        .data_type("String")
        .string_value("base64")
        .build()
        .map_err(backend_error)
}

/// Summarize per-entry batch failures (entry id, error code, and message).
fn batch_failure_message(failed: &[BatchResultErrorEntry]) -> String {
    let details = failed
        .iter()
        .map(|entry| {
            format!(
                "entry {}: {} ({})",
                entry.id(),
                entry.code(),
                entry.message().unwrap_or("no message")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("{} message(s) failed to send: {details}", failed.len())
}

impl MessageQueue for SqsQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        let encoded = encode_message(message);
        let mut request = self
            .client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body(encoded.body);

        if encoded.base64 {
            request = request.message_attributes(CONTENT_ENCODING_ATTRIBUTE, base64_attribute()?);
        }

        request.send().await.map_err(backend_error)?;
        Ok(())
    }

    /// Send a batch of messages in chunks of 10 (the SQS batch maximum).
    ///
    /// Delivery is at-least-once and **not atomic**: when an error is returned
    /// for a later chunk (or for individual entries), messages from earlier
    /// chunks — and the successful entries of the failing chunk — have already
    /// been delivered. Per-entry failures are reported with their entry index,
    /// error code, and message.
    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        // SQS supports up to 10 messages per batch
        for chunk in messages.chunks(10) {
            let mut batch = self.client.send_message_batch().queue_url(&self.queue_url);

            for (i, msg) in chunk.iter().enumerate() {
                let encoded = encode_message(msg);
                let mut entry = aws_sdk_sqs::types::SendMessageBatchRequestEntry::builder()
                    .id(i.to_string())
                    .message_body(encoded.body);
                if encoded.base64 {
                    entry =
                        entry.message_attributes(CONTENT_ENCODING_ATTRIBUTE, base64_attribute()?);
                }
                batch = batch.entries(entry.build().map_err(backend_error)?);
            }

            let result = batch.send().await.map_err(backend_error)?;

            if !result.failed.is_empty() {
                return Err(QueueError::backend(batch_failure_message(&result.failed)));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        batch_failure_message, encode_message, is_sqs_allowed_char, BatchResultErrorEntry,
    };

    #[test]
    fn json_passes_through_unchanged() {
        let payload = br#"{"kind":"email","to":"user@example.com"}"#;
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

    #[test]
    fn batch_failure_message_includes_ids_codes_and_messages() {
        let failed = vec![
            BatchResultErrorEntry::builder()
                .id("3")
                .code("InternalError")
                .message("something broke")
                .sender_fault(false)
                .build()
                .unwrap(),
            BatchResultErrorEntry::builder()
                .id("7")
                .code("InvalidMessageContents")
                .sender_fault(true)
                .build()
                .unwrap(),
        ];

        let message = batch_failure_message(&failed);
        assert!(message.contains("2 message(s) failed"));
        assert!(message.contains("entry 3: InternalError (something broke)"));
        assert!(message.contains("entry 7: InvalidMessageContents (no message)"));
    }
}
