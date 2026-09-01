//! The one runtime representation of a configured secret.
//!
//! A `[[secret]]` entry resolves into exactly this type on every target: an environment variable
//! natively, a Worker secret or a Secrets Store binding on Cloudflare. Nothing else in the
//! framework carries a secret value, so there is a single place where redaction is decided.

use alloc::string::String;
use alloc::sync::Arc;
use core::fmt;

use secrecy::{ExposeSecret, SecretString};

/// A configured secret value.
///
/// The value is readable only through [`Secret::expose`], which is what makes reading one visible
/// in a diff. [`Debug`] prints `Secret(<redacted>)` and there is deliberately no [`Display`]: a
/// secret that formats itself is a secret that reaches a log the first time someone interpolates
/// a struct holding it.
///
/// ```
/// use skyzen_core::Secret;
///
/// let key = Secret::new("sk_live_123");
/// assert_eq!(key.expose(), "sk_live_123");
/// assert_eq!(format!("{key:?}"), "Secret(<redacted>)");
/// ```
#[derive(Clone)]
pub struct Secret(Arc<SecretString>);

impl Secret {
    /// Wrap a value read from the environment, a Worker binding or a secrets store.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(Arc::new(SecretString::from(value.into())))
    }

    /// Read the value.
    ///
    /// This is the only way to reach it, so every use site names the exposure.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;
    use alloc::format;

    const fn assert_send_sync<T: Send + Sync>() {}
    const _: () = assert_send_sync::<Secret>();

    #[test]
    fn debug_redacts_the_value() {
        let secret = Secret::new("hunter2");
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "Secret(<redacted>)");
        assert!(!rendered.contains("hunter2"));
    }

    #[test]
    fn expose_is_the_only_way_to_read_it() {
        assert_eq!(Secret::new("hunter2").expose(), "hunter2");
    }

    #[test]
    fn a_clone_shares_the_value() {
        let secret = Secret::new("hunter2");
        assert_eq!(secret.clone().expose(), secret.expose());
    }
}
