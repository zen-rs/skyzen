//! Cloudflare Queues implementation of [`MessageQueue`].

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use skyzen_services::queue::{MessageQueue, QueueError};

use crate::ffi;

/// A Cloudflare Workers Queue.
///
/// Wraps the Queue binding from the Workers environment.
///
/// # Safety
///
/// WASM in Workers is single-threaded, so `Send` and `Sync` are safe.
pub struct CfQueue {
    queue: ffi::Queue,
}

impl Clone for CfQueue {
    fn clone(&self) -> Self {
        let js: &JsValue = self.queue.as_ref();
        Self {
            queue: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfQueue {}
unsafe impl Sync for CfQueue {}

impl std::fmt::Debug for CfQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfQueue").finish_non_exhaustive()
    }
}

impl CfQueue {
    /// Create a `CfQueue` from a Queue binding.
    ///
    /// # Panics
    ///
    /// Panics if the binding is not a valid Queue.
    #[must_use]
    pub fn new(binding: JsValue) -> Self {
        Self {
            queue: binding.unchecked_into(),
        }
    }

    /// Create a `CfQueue` from a Workers env by binding name.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Backend`] if the binding cannot be found.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, QueueError> {
        let binding = ffi::get_binding(env, binding_name).map_err(|e| {
            QueueError::Backend(format!(
                "failed to get Queue binding '{binding_name}': {e:?}"
            ))
        })?;
        Ok(Self::new(binding))
    }
}

impl MessageQueue for CfQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        let array = js_sys::Uint8Array::from(message);
        let promise = self.queue.send(&array).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)?;
        Ok(())
    }

    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        // WASM is 32-bit, so usize fits in u32
        #[allow(clippy::cast_possible_truncation)]
        let batch = js_sys::Array::new_with_length(messages.len() as u32);
        for (i, msg) in messages.iter().enumerate() {
            let array = js_sys::Uint8Array::from(msg.as_slice());
            let item = js_sys::Object::new();
            js_sys::Reflect::set(&item, &"body".into(), &array)
                .map_err(|e| QueueError::Backend(format!("{e:?}")))?;
            #[allow(clippy::cast_possible_truncation)]
            batch.set(i as u32, item.into());
        }

        let promise = self.queue.send_batch(&batch).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)?;
        Ok(())
    }
}

/// Convert a `JsValue` error to a `QueueError`.
///
/// Takes ownership to match `Result<_, JsValue>::map_err` signature.
#[allow(clippy::needless_pass_by_value)]
fn js_err(e: JsValue) -> QueueError {
    QueueError::Backend(format!("{e:?}"))
}
