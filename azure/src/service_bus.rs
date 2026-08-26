//! Azure Service Bus implementation of [`MessageQueue`].

use azure_messaging_servicebus::service_bus::QueueClient;
use base64::Engine;
use skyzen_services::queue::{MessageQueue, QueueError};

/// An Azure Service Bus-backed message queue.
///
/// Wraps the Azure SDK's Service Bus queue client to implement [`MessageQueue`].
///
/// # Wire format
///
/// Payloads that are valid UTF-8 are sent **verbatim**, so JSON produced by
/// `send_json` arrives as plain JSON — the same wire contract as the
/// Cloudflare and in-memory backends. Binary (non-UTF-8) payloads are
/// base64-encoded because the underlying SDK only accepts strings; consumers
/// of mixed text/binary queues must be prepared to base64-decode such bodies
/// (the legacy SDK offers no message-property channel to tag them).
///
/// # Batch semantics
///
/// The legacy Service Bus SDK has no batch send API, so
/// [`MessageQueue::send_batch`] sends messages **sequentially**, one request
/// per message. Delivery is not atomic: when a send fails, every earlier
/// message in the batch has already been delivered and the remainder is not
/// attempted.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Debug, Clone)]
pub struct ServiceBusQueue {
    client: QueueClient,
}

impl ServiceBusQueue {
    /// Create a new `ServiceBusQueue` from an existing [`QueueClient`].
    ///
    /// # Example
    ///
    /// ```ignore
    /// use azure_messaging_servicebus::service_bus::QueueClient;
    ///
    /// let http_client = azure_core::new_http_client();
    /// let client = QueueClient::new(
    ///     http_client,
    ///     "my-namespace",
    ///     "my-queue",
    ///     "policy-name",
    ///     "signing-key",
    /// );
    /// let queue = ServiceBusQueue::new(client);
    /// ```
    #[must_use]
    pub const fn new(client: QueueClient) -> Self {
        Self { client }
    }
}

/// Encode a payload for Service Bus: verbatim when it is valid UTF-8,
/// base64 otherwise (the SDK's send API only accepts `&str`).
fn encode_body(message: &[u8]) -> String {
    core::str::from_utf8(message).map_or_else(
        |_| base64::engine::general_purpose::STANDARD.encode(message),
        ToOwned::to_owned,
    )
}

impl MessageQueue for ServiceBusQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        let body = encode_body(message);
        self.client
            .send_message(&body, None)
            .await
            .map_err(|error| QueueError::backend_with(error.to_string(), error))?;
        Ok(())
    }

    /// Send messages sequentially (the SDK has no batch API).
    ///
    /// Not atomic: on failure, messages before the failing one have already
    /// been delivered (at-least-once), and later ones are not attempted.
    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        for (index, msg) in messages.iter().enumerate() {
            self.send(msg).await.map_err(|error| {
                QueueError::backend(format!(
                    "batch send failed at message {index} of {}: {error} \
                     (messages before index {index} were already delivered)",
                    messages.len()
                ))
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::encode_body;
    use base64::Engine;

    #[test]
    fn json_passes_through_unchanged() {
        let payload = br#"{"kind":"email"}"#;
        assert_eq!(encode_body(payload).as_bytes(), payload);
    }

    #[test]
    fn unicode_text_passes_through_unchanged() {
        let payload = "hello 世界".as_bytes();
        assert_eq!(encode_body(payload).as_bytes(), payload);
    }

    #[test]
    fn binary_payload_is_base64_encoded() {
        let payload = [0xFF, 0xFE, 0x00, 0x01];
        let body = encode_body(&payload);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&body)
            .unwrap();
        assert_eq!(decoded, payload);
    }
}
