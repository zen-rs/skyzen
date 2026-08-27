//! Resource ids: which bindings have one, and what to do when they do not.
//!
//! KV namespaces and D1 databases are addressed by an opaque id that only Cloudflare can mint, so
//! `Skyzen.toml` leaves them unset until `skyzen provision` fills them in. R2 buckets and queues
//! are addressed by name, so they need creating but never a write-back.

use crate::providers::cloudflare::wrangler::IdPolicy;
use anyhow::Result;

/// The kinds of Cloudflare resource Skyzen can provision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResourceKind {
    /// A KV namespace, identified by an opaque id.
    KvNamespace,
    /// A D1 database, identified by an opaque id.
    D1Database,
    /// An R2 bucket, identified by its name.
    R2Bucket,
    /// A queue, identified by its name.
    Queue,
}

impl ResourceKind {
    /// The wrangler subcommand that creates one, as separate arguments.
    pub const fn create_subcommand(self) -> &'static [&'static str] {
        match self {
            Self::KvNamespace => &["kv", "namespace", "create"],
            Self::D1Database => &["d1", "create"],
            Self::R2Bucket => &["r2", "bucket", "create"],
            Self::Queue => &["queues", "create"],
        }
    }

    /// A human name for progress and error messages.
    pub const fn label(self) -> &'static str {
        match self {
            Self::KvNamespace => "KV namespace",
            Self::D1Database => "D1 database",
            Self::R2Bucket => "R2 bucket",
            Self::Queue => "queue",
        }
    }

    /// Whether creating one yields an id that has to be written back into `Skyzen.toml`.
    pub const fn yields_id(self) -> bool {
        matches!(self, Self::KvNamespace | Self::D1Database)
    }
}

/// One binding that may need a resource created for it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot {
    /// What kind of resource backs the binding.
    pub kind: ResourceKind,
    /// The binding name, which is how the slot is matched to a manifest entry.
    pub binding: String,
    /// The name handed to the wrangler create subcommand.
    pub resource_name: String,
}

impl Slot {
    /// A KV namespace binding. Wrangler names the namespace after the binding.
    pub fn kv(binding: &str) -> Self {
        Self {
            kind: ResourceKind::KvNamespace,
            binding: binding.to_owned(),
            resource_name: binding.to_owned(),
        }
    }

    /// A D1 database binding, created under the manifest's `database_name`.
    pub fn d1(binding: &str, database_name: &str) -> Self {
        Self {
            kind: ResourceKind::D1Database,
            binding: binding.to_owned(),
            resource_name: database_name.to_owned(),
        }
    }

    /// An R2 bucket binding, created under the manifest's `bucket_name`.
    pub fn r2(binding: &str, bucket_name: &str) -> Self {
        Self {
            kind: ResourceKind::R2Bucket,
            binding: binding.to_owned(),
            resource_name: bucket_name.to_owned(),
        }
    }

    /// A queue, created under its own name. Consumers have no binding, so the queue names itself.
    pub fn queue(binding: Option<&str>, queue: &str) -> Self {
        Self {
            kind: ResourceKind::Queue,
            binding: binding.unwrap_or(queue).to_owned(),
            resource_name: queue.to_owned(),
        }
    }

    /// The id a local `wrangler dev` run stands in with.
    ///
    /// Deterministic, so the same binding keys the same miniflare storage across runs, and
    /// obviously not a real id, so it can never be mistaken for a provisioned one.
    pub fn local_placeholder(&self) -> String {
        let slug: String = self
            .binding
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        format!("skyzen-local-{slug}")
    }
}

/// Whether a manifest id field counts as provisioned.
pub fn is_provisioned(id: Option<&str>) -> bool {
    id.is_some_and(|id| !id.trim().is_empty())
}

/// The id to render for a binding, according to the build's [`IdPolicy`].
///
/// # Errors
///
/// Under [`IdPolicy::RequireProvisioned`], fails naming the binding and the command that fills it
/// in. A deploy must never bind to an invented id.
pub fn resolve(id: Option<&str>, slot: &Slot, policy: IdPolicy) -> Result<String> {
    if is_provisioned(id) {
        return Ok(id.unwrap_or_default().trim().to_owned());
    }

    match policy {
        IdPolicy::LocalPlaceholder => Ok(slot.local_placeholder()),
        IdPolicy::RequireProvisioned => anyhow::bail!(
            "the {} bound as `{}` has no id in Skyzen.toml. Run `skyzen provision --provider cloudflare` \
             to create it, or paste an existing id.",
            slot.kind.label(),
            slot.binding
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_provisioned, resolve, ResourceKind, Slot};
    use crate::providers::cloudflare::wrangler::IdPolicy;

    #[test]
    fn a_blank_id_is_not_provisioned() {
        assert!(!is_provisioned(None));
        assert!(!is_provisioned(Some("")));
        assert!(!is_provisioned(Some("   ")));
        assert!(is_provisioned(Some("abc123")));
    }

    #[test]
    fn a_provisioned_id_is_used_verbatim_under_either_policy() {
        let slot = Slot::kv("CACHE");
        for policy in [IdPolicy::LocalPlaceholder, IdPolicy::RequireProvisioned] {
            assert_eq!(
                resolve(Some(" abc123 "), &slot, policy).expect("id"),
                "abc123"
            );
        }
    }

    #[test]
    fn the_local_placeholder_is_deterministic_and_unmistakable() {
        let slot = Slot::kv("MY_CACHE");
        assert_eq!(slot.local_placeholder(), slot.local_placeholder());
        assert_eq!(slot.local_placeholder(), "skyzen-local-my-cache");
        assert_ne!(
            slot.local_placeholder(),
            Slot::kv("OTHER").local_placeholder()
        );
    }

    #[test]
    fn only_id_bearing_resources_are_written_back() {
        assert!(ResourceKind::KvNamespace.yields_id());
        assert!(ResourceKind::D1Database.yields_id());
        assert!(!ResourceKind::R2Bucket.yields_id());
        assert!(!ResourceKind::Queue.yields_id());
    }
}
