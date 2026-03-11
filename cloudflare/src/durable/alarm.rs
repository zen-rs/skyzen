//! Cloudflare Durable Object alarm adapter for [`AlarmScheduler`].

use skyzen_services::durable::alarm::{AlarmError, AlarmScheduler};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker_sys::{DurableObjectState, DurableObjectStorage};

/// Cloudflare Durable Object alarm scheduler backed by `state.storage`.
pub struct CfAlarm {
    storage: DurableObjectStorage,
}

impl Clone for CfAlarm {
    fn clone(&self) -> Self {
        let js: &JsValue = self.storage.as_ref();
        Self {
            storage: js.clone().unchecked_into(),
        }
    }
}

unsafe impl Send for CfAlarm {}
unsafe impl Sync for CfAlarm {}

impl std::fmt::Debug for CfAlarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfAlarm").finish_non_exhaustive()
    }
}

impl CfAlarm {
    /// Create from a Durable Object storage handle.
    #[must_use]
    pub const fn new(storage: DurableObjectStorage) -> Self {
        Self { storage }
    }

    /// Create from Durable Object state.
    ///
    /// # Errors
    ///
    /// Returns [`AlarmError`] if `state.storage` cannot be read.
    pub fn from_state(state: &DurableObjectState) -> Result<Self, AlarmError> {
        let storage = state.storage().map_err(js_err)?;
        Ok(Self::new(storage))
    }
}

impl AlarmScheduler for CfAlarm {
    async fn get_alarm(&self) -> Result<Option<i64>, AlarmError> {
        let options = js_sys::Object::new();
        let promise = self.storage.get_alarm(options).map_err(js_err)?;
        let value = JsFuture::from(promise).await.map_err(js_err)?;

        if value.is_null() || value.is_undefined() {
            return Ok(None);
        }

        let timestamp = value.as_f64().ok_or_else(|| {
            AlarmError::Backend(format!(
                "DurableObjectStorage.getAlarm returned non-number: {value:?}"
            ))
        })?;
        if !timestamp.is_finite() || timestamp.fract() != 0.0 {
            return Err(AlarmError::Backend(format!(
                "DurableObjectStorage.getAlarm returned invalid timestamp: {timestamp}"
            )));
        }

        #[allow(clippy::cast_possible_truncation)]
        Ok(Some(timestamp as i64))
    }

    async fn set_alarm(&self, scheduled_time_ms: i64) -> Result<(), AlarmError> {
        #[allow(clippy::cast_precision_loss)]
        let when = js_sys::Date::new(&JsValue::from_f64(scheduled_time_ms as f64));
        let options = js_sys::Object::new();
        let promise = self.storage.set_alarm(when, options).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)?;
        Ok(())
    }

    async fn delete_alarm(&self) -> Result<(), AlarmError> {
        let options = js_sys::Object::new();
        let promise = self.storage.delete_alarm(options).map_err(js_err)?;
        JsFuture::from(promise).await.map_err(js_err)?;
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value)]
fn js_err(error: JsValue) -> AlarmError {
    AlarmError::Backend(format!("{error:?}"))
}
