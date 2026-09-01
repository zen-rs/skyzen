//! Readers for the two shapes a Cloudflare Workers secret binding can take.
//!
//! Cloudflare has two unrelated shapes here, and reading one with the other's API fails in a way
//! that is hard to diagnose:
//!
//! - **`vars` and classic per-Worker secrets** are plain JS *strings* on the `env` object.
//!   [`CfSecret::classic`] reads those.
//! - **Secrets Store bindings** are *objects* whose value is fetched asynchronously via `.get()`,
//!   so the string reader sees a non-string and reports [`CfSecretError::NotString`].
//!   [`CfSecret::from_store`] reads those.
//!
//! A handler normally reaches neither: `import_config!` generates one named
//! [`skyzen::Secret`]-carrying type per `[[secret]]` entry and picks the right reader from the
//! manifest, so the binding name is checked at compile time instead of being repeated as a string
//! literal. These functions are what that generated code calls.
//!
//! `vars` are rendered into the generated `wrangler.toml` in plaintext and belong in source
//! control only if their contents do; a real secret is declared as `[[secret]]` and pushed with
//! `skyzen secret push` or backed by the Secrets Store.

use js_sys::Reflect;
use skyzen::Secret;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::send::IntoSendFuture;
use worker_sys::SecretStoreSys;

/// Errors raised when reading a Cloudflare Workers secret binding.
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
    /// A Secrets Store binding lands here when read with [`CfSecret::classic`]: it is an object,
    /// not a string. Read it with [`CfSecret::from_store`] instead.
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

/// The reader for a Cloudflare Workers secret binding.
///
/// Both constructors return a [`skyzen::Secret`], so a value read out of the Workers environment
/// is redacted from the moment it exists.
///
/// # Example
///
/// ```ignore
/// let classic = CfSecret::classic(&env, "JWT_SIGNING_KEY")?;
/// let stored = CfSecret::from_store(&env, "STRIPE_KEY").await?;
/// ```
#[derive(Debug)]
pub struct CfSecret;

impl CfSecret {
    /// Read a classic per-Worker secret (or a `vars` entry): a plain string on `env`.
    ///
    /// # Errors
    ///
    /// Returns [`CfSecretError`] when the binding cannot be read, is missing, or is not a string —
    /// the last being what a Secrets Store binding looks like here, which
    /// [`CfSecret::from_store`] reads instead.
    pub fn classic(env: &JsValue, binding: &str) -> Result<Secret, CfSecretError> {
        let value =
            Reflect::get(env, &JsValue::from_str(binding)).map_err(|_| CfSecretError::Reflect {
                binding: binding.to_owned(),
            })?;
        if value.is_undefined() || value.is_null() {
            return Err(CfSecretError::Missing {
                binding: binding.to_owned(),
            });
        }
        value
            .as_string()
            .map(Secret::new)
            .ok_or_else(|| CfSecretError::NotString {
                binding: binding.to_owned(),
            })
    }

    /// Read a secret out of a Secrets Store binding.
    ///
    /// Unlike a classic secret, which the runtime materializes as a string on `env` before the
    /// Worker starts, a Secrets Store secret is fetched on demand — so reading one is `async` and
    /// can fail.
    ///
    /// # Errors
    ///
    /// Returns [`CfSecretError`] when the binding is missing, is not a Secrets Store binding (the
    /// common case being a classic secret, which [`CfSecret::classic`] reads instead), when the
    /// store refuses the read, or when it hands back something that is not a string.
    pub async fn from_store(env: &JsValue, binding: &str) -> Result<Secret, CfSecretError> {
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

        secret
            .as_string()
            .map(Secret::new)
            .ok_or_else(|| CfSecretError::NotString {
                binding: binding.to_owned(),
            })
    }
}
