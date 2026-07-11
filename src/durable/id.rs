//! Durable Object identity.

/// The unique identity of a Durable Object instance.
///
/// This is an opaque wrapper around the platform-specific ID.
/// On Cloudflare, it wraps `DurableObjectId` from `worker-sys`.
#[derive(Debug, Clone)]
pub struct DurableObjectId {
    /// The hex string representation of the ID.
    id: String,
    /// The optional name (if created via `idFromName`).
    name: Option<String>,
}

impl DurableObjectId {
    /// Create a new `DurableObjectId` from its string representation.
    #[must_use]
    pub const fn new(id: String, name: Option<String>) -> Self {
        Self { id, name }
    }

    /// The hex string representation of this ID.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.id
    }

    /// The name used to derive this ID, if any.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

impl std::fmt::Display for DurableObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.id)
    }
}
