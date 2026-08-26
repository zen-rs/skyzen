//! Cloudflare Durable Object storage adapter for [`DurableKvStore`].

use skyzen_services::durable::kv::{DurableKvError, DurableKvStore, DurableListOptions};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::send::IntoSendFuture;
use worker_sys::{DurableObjectState, DurableObjectStorage};

/// Cloudflare Durable Object KV store backed by `state.storage`.
///
/// The framework's internal state key ([`super::STATE_KEY`]) shares this
/// keyspace: it is filtered out of `list` results and survives `delete_all`,
/// so user code never sees or destroys framework state.
pub struct CfDurableKv {
    storage: DurableObjectStorage,
}

impl_js_handle_traits!(CfDurableKv { storage });

impl CfDurableKv {
    /// Create from a Durable Object storage handle.
    #[must_use]
    pub const fn new(storage: DurableObjectStorage) -> Self {
        Self { storage }
    }

    /// Create from Durable Object state.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if `state.storage` cannot be read.
    pub fn from_state(state: &DurableObjectState) -> Result<Self, DurableKvError> {
        let storage = state.storage().map_err(js_err)?;
        Ok(Self::new(storage))
    }
}

impl DurableKvStore for CfDurableKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, DurableKvError> {
        let promise = self.storage.get(key).map_err(js_err)?;
        let value = JsFuture::from(promise).into_send().await.map_err(js_err)?;
        decode_optional_bytes(&value)
    }

    async fn get_multiple(&self, keys: &[&str]) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
        let keys = keys
            .iter()
            .map(|key| JsValue::from_str(key))
            .collect::<Vec<_>>();
        let promise = self.storage.get_multiple(keys).map_err(js_err)?;
        let value = JsFuture::from(promise).into_send().await.map_err(js_err)?;
        decode_map_entries(value)
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), DurableKvError> {
        let bytes = js_sys::Uint8Array::from(value);
        let promise = self.storage.put(key, bytes.into()).map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    async fn put_multiple(&self, entries: &[(&str, &[u8])]) -> Result<(), DurableKvError> {
        let object = js_sys::Object::new();
        for (key, value) in entries {
            let bytes = js_sys::Uint8Array::from(*value);
            let bytes_value: JsValue = bytes.into();
            js_sys::Reflect::set(&object, &JsValue::from_str(key), &bytes_value).map_err(js_err)?;
        }
        let promise = self.storage.put_multiple(object.into()).map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<bool, DurableKvError> {
        let promise = self.storage.delete(key).map_err(js_err)?;
        let value = JsFuture::from(promise).into_send().await.map_err(js_err)?;
        value.as_bool().ok_or_else(|| {
            DurableKvError::backend(format!(
                "DurableObjectStorage.delete returned non-boolean value: {value:?}"
            ))
        })
    }

    async fn delete_multiple(&self, keys: &[&str]) -> Result<usize, DurableKvError> {
        let keys = keys
            .iter()
            .map(|key| JsValue::from_str(key))
            .collect::<Vec<_>>();
        let promise = self.storage.delete_multiple(keys).map_err(js_err)?;
        let value = JsFuture::from(promise).into_send().await.map_err(js_err)?;
        to_usize(&value, "DurableObjectStorage.deleteMultiple")
    }

    async fn delete_all(&self) -> Result<(), DurableKvError> {
        // `deleteAll` would also wipe the framework's serialized object state
        // mid-session; snapshot it and restore it afterwards.
        let state_snapshot = self.get(super::STATE_KEY).await?;
        let promise = self.storage.delete_all().map_err(js_err)?;
        JsFuture::from(promise).into_send().await.map_err(js_err)?;
        if let Some(bytes) = state_snapshot {
            self.put(super::STATE_KEY, &bytes).await?;
        }
        Ok(())
    }

    async fn list(
        &self,
        options: DurableListOptions<'_>,
    ) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
        let js_options = js_sys::Object::new();
        if let Some(prefix) = options.prefix {
            js_sys::Reflect::set(
                &js_options,
                &JsValue::from_str("prefix"),
                &JsValue::from_str(prefix),
            )
            .map_err(js_err)?;
        }
        if let Some(start) = options.start {
            // The trait documents `start` as exclusive; Cloudflare's `start`
            // option is inclusive, so map it to `startAfter`.
            js_sys::Reflect::set(
                &js_options,
                &JsValue::from_str("startAfter"),
                &JsValue::from_str(start),
            )
            .map_err(js_err)?;
        }
        if let Some(end) = options.end {
            js_sys::Reflect::set(
                &js_options,
                &JsValue::from_str("end"),
                &JsValue::from_str(end),
            )
            .map_err(js_err)?;
        }
        if let Some(limit) = options.limit {
            #[allow(clippy::cast_precision_loss)]
            let limit = JsValue::from_f64(limit as f64);
            js_sys::Reflect::set(&js_options, &JsValue::from_str("limit"), &limit)
                .map_err(js_err)?;
        }
        if options.reverse {
            js_sys::Reflect::set(
                &js_options,
                &JsValue::from_str("reverse"),
                &JsValue::from_bool(true),
            )
            .map_err(js_err)?;
        }

        let promise = self.storage.list_with_options(js_options).map_err(js_err)?;
        let value = JsFuture::from(promise).into_send().await.map_err(js_err)?;
        let mut entries = decode_map_entries(value)?;
        // Hide the framework's internal state key from user-visible listings.
        entries.retain(|(key, _)| key != super::STATE_KEY);
        Ok(entries)
    }
}

fn decode_map_entries(value: JsValue) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
    let map: js_sys::Map = value.dyn_into().map_err(|value| {
        DurableKvError::backend(format!(
            "DurableObjectStorage returned non-Map list result: {value:?}"
        ))
    })?;

    let mut entries = Vec::new();
    let iter = js_sys::try_iter(map.as_ref())
        .map_err(js_err)?
        .ok_or_else(|| DurableKvError::backend("Map iterator unavailable"))?;

    for entry in iter {
        let entry = entry.map_err(js_err)?;
        let pair = js_sys::Array::from(&entry);
        if pair.length() != 2 {
            return Err(DurableKvError::backend(
                "Map entry must contain [key, value]".to_owned(),
            ));
        }

        let key = pair.get(0).as_string().ok_or_else(|| {
            DurableKvError::backend(format!("Map entry key is not string: {:?}", pair.get(0)))
        })?;
        let value = pair.get(1);
        let value = decode_required_bytes(&value)?;
        entries.push((key, value));
    }

    Ok(entries)
}

fn decode_optional_bytes(value: &JsValue) -> Result<Option<Vec<u8>>, DurableKvError> {
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    decode_required_bytes(value).map(Some)
}

fn decode_required_bytes(value: &JsValue) -> Result<Vec<u8>, DurableKvError> {
    if value.is_instance_of::<js_sys::Uint8Array>() {
        return Ok(js_sys::Uint8Array::new(value).to_vec());
    }
    if value.is_instance_of::<js_sys::ArrayBuffer>() {
        return Ok(js_sys::Uint8Array::new(value).to_vec());
    }
    Err(DurableKvError::backend(format!(
        "Expected Uint8Array/ArrayBuffer from DurableObjectStorage, got {value:?}"
    )))
}

fn to_usize(value: &JsValue, source: &str) -> Result<usize, DurableKvError> {
    let number = value.as_f64().ok_or_else(|| {
        DurableKvError::backend(format!("{source} returned non-number value: {value:?}"))
    })?;
    if !number.is_finite() || number < 0.0 || number.fract() != 0.0 {
        return Err(DurableKvError::backend(format!(
            "{source} returned invalid count value: {number}"
        )));
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        Ok(number as usize)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn js_err(error: JsValue) -> DurableKvError {
    DurableKvError::backend(format!("{error:?}"))
}
