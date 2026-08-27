//! Cloudflare Queues implementation of [`MessageQueue`].

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use worker::send::IntoSendFuture;

use skyzen_services::queue::{MessageQueue, QueueError, SendOptions};

/// Cloudflare Queues caps a producer-side delay at 12 hours.
const MAX_DELAY_SECS: u64 = 43_200;

/// A Cloudflare Workers Queue.
///
/// Wraps the Queue binding from the Workers environment.
///
/// Messages are sent with `contentType: "bytes"` so the raw payload reaches
/// the consumer as an `ArrayBuffer` instead of being JSON-mangled by the
/// platform default content type.
///
/// # Safety
///
/// WASM in Workers is single-threaded, so `Send` and `Sync` are safe.
pub struct CfQueue {
    queue: worker_sys::Queue,
}

impl_js_handle_traits!(CfQueue { queue });

impl CfQueue {
    /// Create a `CfQueue` from a Queue binding.
    ///
    /// The binding is not validated here; an invalid binding surfaces as a
    /// JS error on first use. Prefer [`CfQueue::from_env`], which checks
    /// that the binding looks like a Queue.
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
    /// Returns [`QueueError::Backend`] if the binding cannot be found or
    /// does not look like a Queue.
    pub fn from_env(env: &JsValue, binding_name: &str) -> Result<Self, QueueError> {
        let binding = crate::ffi::get_binding(env, binding_name).map_err(|e| {
            QueueError::backend(format!(
                "failed to get Queue binding '{binding_name}': {e:?}"
            ))
        })?;
        crate::ffi::require_methods(&binding, binding_name, &["send", "sendBatch"])
            .map_err(js_err)?;
        Ok(Self::new(binding))
    }
}

/// Build a `{ contentType: "bytes" }` send-options object.
///
/// Cloudflare's default `contentType` is `"json"`, which would JSON-stringify
/// a `Uint8Array` into index-keyed garbage; `"bytes"` delivers the payload to
/// the consumer as an `ArrayBuffer`.
fn bytes_content_type_options() -> Result<js_sys::Object, QueueError> {
    let options = js_sys::Object::new();
    js_sys::Reflect::set(&options, &"contentType".into(), &"bytes".into()).map_err(js_err)?;
    Ok(options)
}

impl MessageQueue for CfQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        // `Uint8Array::from` copies into an exactly sized buffer, so the
        // ArrayBuffer holds precisely the message bytes.
        let buffer = js_sys::Uint8Array::from(message).buffer();
        let options = bytes_content_type_options()?;
        let promise = self
            .queue
            .send(buffer.into(), options.into())
            .map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    /// Send one message, optionally holding it invisible for a while first.
    ///
    /// Cloudflare's `delaySeconds` is whole seconds, so a sub-second delay is rounded *up*: a
    /// message asked to wait must not become visible early.
    async fn send_with(&self, message: &[u8], options: SendOptions) -> Result<(), QueueError> {
        let js_options = bytes_content_type_options()?;
        if let Some(delay) = options.delay {
            let mut seconds = delay.as_secs();
            if delay.subsec_nanos() > 0 {
                seconds = seconds.saturating_add(1);
            }
            if seconds > MAX_DELAY_SECS {
                return Err(QueueError::backend(format!(
                    "Cloudflare Queues delays a message by at most {MAX_DELAY_SECS} seconds \
                     (12 hours); got {seconds}"
                )));
            }
            #[allow(clippy::cast_precision_loss)]
            js_sys::Reflect::set(
                &js_options,
                &"delaySeconds".into(),
                &JsValue::from_f64(seconds as f64),
            )
            .map_err(js_err)?;
        }

        let buffer = js_sys::Uint8Array::from(message).buffer();
        let promise = self
            .queue
            .send(buffer.into(), js_options.into())
            .map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        // WASM is 32-bit, so usize fits in u32
        #[allow(clippy::cast_possible_truncation)]
        let batch = js_sys::Array::new_with_length(messages.len() as u32);
        for (i, msg) in messages.iter().enumerate() {
            let buffer = js_sys::Uint8Array::from(msg.as_slice()).buffer();
            let item = js_sys::Object::new();
            js_sys::Reflect::set(&item, &"body".into(), &buffer).map_err(js_err)?;
            js_sys::Reflect::set(&item, &"contentType".into(), &"bytes".into()).map_err(js_err)?;
            #[allow(clippy::cast_possible_truncation)]
            batch.set(i as u32, item.into());
        }

        let promise = self
            .queue
            .send_batch(batch, JsValue::UNDEFINED)
            .map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }
}

/// Convert a `JsValue` error to a `QueueError`.
///
/// Takes ownership to match `Result<_, JsValue>::map_err` signature.
#[allow(clippy::needless_pass_by_value)]
fn js_err(e: JsValue) -> QueueError {
    QueueError::backend(format!("{e:?}"))
}
