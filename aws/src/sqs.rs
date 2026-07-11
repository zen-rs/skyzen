//! Amazon SQS implementation of [`MessageQueue`].

use aws_sdk_sqs::Client;
use skyzen_services::queue::{MessageQueue, QueueError};

/// An Amazon SQS-backed message queue.
///
/// Messages are sent as base64-encoded strings since SQS messages are text-based.
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

/// Encode bytes to base64 for SQS transport.
fn encode_message(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

impl MessageQueue for SqsQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        self.client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body(encode_message(message))
            .send()
            .await
            .map_err(|e| QueueError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        // SQS supports up to 10 messages per batch
        for chunk in messages.chunks(10) {
            let mut batch = self.client.send_message_batch().queue_url(&self.queue_url);

            for (i, msg) in chunk.iter().enumerate() {
                let entry = aws_sdk_sqs::types::SendMessageBatchRequestEntry::builder()
                    .id(i.to_string())
                    .message_body(encode_message(msg))
                    .build()
                    .map_err(|e| QueueError::Backend(e.to_string()))?;
                batch = batch.entries(entry);
            }

            let result = batch
                .send()
                .await
                .map_err(|e| QueueError::Backend(e.to_string()))?;

            if !result.failed.is_empty() {
                return Err(QueueError::Backend(format!(
                    "{} messages failed to send",
                    result.failed.len()
                )));
            }
        }

        Ok(())
    }
}
