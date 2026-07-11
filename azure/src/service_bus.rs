//! Azure Service Bus implementation of [`MessageQueue`].

use azure_messaging_servicebus::service_bus::QueueClient;
use base64::Engine;
use skyzen_services::queue::{MessageQueue, QueueError};

/// An Azure Service Bus-backed message queue.
///
/// Wraps the Azure SDK's Service Bus queue client to implement [`MessageQueue`].
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

impl MessageQueue for ServiceBusQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        // Service Bus send_message expects a &str, so base64-encode the bytes
        let encoded = base64::engine::general_purpose::STANDARD.encode(message);
        self.client
            .send_message(&encoded, None)
            .await
            .map_err(|e| QueueError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        // Service Bus Rust SDK does not support batch sends,
        // so we send messages individually.
        for msg in messages {
            self.send(msg).await?;
        }
        Ok(())
    }
}
