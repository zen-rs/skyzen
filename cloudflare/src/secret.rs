//! Typed readers for Cloudflare Workers secret and variable bindings.
//!
//! Cloudflare has two unrelated shapes here, and reading one with the other's API fails in a way
//! that is hard to diagnose:
//!
//! - **`vars` and classic per-Worker secrets** are plain JS *strings* on the `env` object.
//!   [`required_string`] and [`optional_string`] read those.
//! - **Secrets Store bindings** are *objects* whose value is fetched asynchronously via `.get()`,
//!   so the string readers see a non-string and report [`CfSecretError::NotString`].
//!   [`CfSecretStore`] reads those.
//!
//! `vars` are rendered into the generated `wrangler.toml` in plaintext and belong in source
//! control only if their contents do; a real secret goes through `wrangler secret put` or the
//! Secrets Store.

use js_sys::Reflect;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::send::IntoSendFuture;
use worker_sys::SecretStoreSys;

/// Errors raised when reading a Cloudflare Workers string binding.
#[derive(Debug, thiserror::Error)]
pub enum CfSecretError {
    /// `Reflect::get` failed (typically: `env` is not the expected object).
    #[error("read Cloudflare Workers binding `{binding}`")]
    Reflect {
        /// Binding name being read.
        binding: String,
    },
    /// The binding is missing (undefined or null).
    #[error("Cloudflare Workers binding `{binding}` is missing")]
    Missing {
        /// Binding name being read.
        binding: String,
    },
    /// The binding exists but isn't a string value.
    ///
    /// A Secrets Store binding lands here when read with [`required_string`]: it is an object, not
    /// a string. Read it with [`CfSecretStore`] instead.
    #[error("Cloudflare Workers binding `{binding}` must be a string")]
    NotString {
        /// Binding name being read.
        binding: String,
    },
    /// The binding exists but does not expose the Secrets Store `get` method.
    #[error("Cloudflare Workers binding `{binding}` is not a Secrets Store binding")]
    NotSecretStore {
        /// Binding name being read.
        binding: String,
    },
    /// The Secrets Store refused the read.
    #[error("Cloudflare Secrets Store rejected the read of `{binding}`: {message}")]
    Read {
        /// Binding name being read.
        binding: String,
        /// The runtime's own message.
        message: String,
    },
}

/// A Cloudflare Secrets Store binding.
///
/// Unlike a classic secret, which the runtime materializes as a string on `env` before the Worker
/// starts, a Secrets Store secret is fetched on demand — so reading one is `async` and can fail.
///
/// # Example
///
/// ```ignore
/// let api_key = CfSecretStore::get(&env, "STRIPE_KEY").await?;
/// ```
#[derive(Debug)]
pub struct CfSecretStore;

impl CfSecretStore {
    /// Read a secret out of a Secrets Store binding.
    ///
    /// # Errors
    ///
    /// Returns [`CfSecretError`] when the binding is missing, is not a Secrets Store binding (the
    /// common case being a classic secret, which [`required_string`] reads instead), when the
    /// store refuses the read, or when it hands back something that is not a string.
    pub async fn get(env: &JsValue, binding: &str) -> Result<String, CfSecretError> {
        let value =
            Reflect::get(env, &JsValue::from_str(binding)).map_err(|_| CfSecretError::Reflect {
                binding: binding.to_owned(),
            })?;
        if value.is_undefined() || value.is_null() {
            return Err(CfSecretError::Missing {
                binding: binding.to_owned(),
            });
        }

        // Duck-type before casting, so a classic secret wired to a Secrets Store reader is named
        // here instead of throwing an opaque `value.get is not a function` on first use.
        let getter = Reflect::get(&value, &JsValue::from_str("get")).map_err(|_| {
            CfSecretError::NotSecretStore {
                binding: binding.to_owned(),
            }
        })?;
        if !getter.is_function() {
            return Err(CfSecretError::NotSecretStore {
                binding: binding.to_owned(),
            });
        }

        let store: SecretStoreSys = value.unchecked_into();
        let promise = store.get().map_err(|error| CfSecretError::Read {
            binding: binding.to_owned(),
            message: format!("{error:?}"),
        })?;
        let secret =
            JsFuture::from(promise)
                .into_send()
                .await
                .map_err(|error| CfSecretError::Read {
                    binding: binding.to_owned(),
                    message: format!("{error:?}"),
                })?;

        secret.as_string().ok_or_else(|| CfSecretError::NotString {
            binding: binding.to_owned(),
        })
    }
}

/// Read a required string binding. Returns an error when the binding is
/// missing or non-string.
///
/// # Errors
///
/// Returns [`CfSecretError`] when the binding cannot be read, is missing,
/// or is not a string.
pub fn required_string(env: &JsValue, binding: &str) -> Result<String, CfSecretError> {
    let value =
        Reflect::get(env, &JsValue::from_str(binding)).map_err(|_| CfSecretError::Reflect {
            binding: binding.to_owned(),
        })?;
    if value.is_undefined() || value.is_null() {
        return Err(CfSecretError::Missing {
            binding: binding.to_owned(),
        });
    }
    value.as_string().ok_or_else(|| CfSecretError::NotString {
        binding: binding.to_owned(),
    })
}

/// Read an optional string binding. Returns `Ok(None)` when the binding is
/// undefined or null. Returns `Err` only when the binding is present but not
/// a string.
///
/// # Errors
///
/// Returns [`CfSecretError::NotString`] when the binding is present but not
/// a string.
pub fn optional_string(env: &JsValue, binding: &str) -> Result<Option<String>, CfSecretError> {
    let Ok(value) = Reflect::get(env, &JsValue::from_str(binding)) else {
        return Ok(None);
    };
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value
        .as_string()
        .map(Some)
        .ok_or_else(|| CfSecretError::NotString {
            binding: binding.to_owned(),
        })
}
