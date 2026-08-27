//! The capability catalogue: which crates a manifest declaration needs.
//!
//! This table is the only place that knows a `backend = "redis"` needs `skyzen-redis`. Two
//! commands read it: `skyzen add`, which hands the entries to `cargo add`, and `dev`/`deploy`,
//! which check the project already has them and fail with the exact command to run if not.
//!
//! `cargo add` rather than a hand-written version requirement is deliberate. The CLI used to
//! rewrite the user's `Cargo.toml` as a silent side effect of `dev`/`deploy`, stamping
//! `env!("CARGO_PKG_VERSION")` — *its own* version — onto every framework crate, which is
//! unresolvable the moment the CLI's version and a service crate's diverge. Delegating to cargo
//! means the versions are whatever the registry actually has, and the edit is a command the user
//! ran on purpose.

use crate::{output, project::Project};
use anyhow::{Context, Result};
use skyzen_manifest::{NativeDatabaseBackend, NativeServiceBackend, ServiceType, SkyzenManifest};
use std::{
    collections::BTreeSet,
    path::Path,
    process::{Command, Stdio},
};

/// One crate a capability pulls in, with the features that capability needs from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CrateRequirement {
    /// The crate name, as published.
    pub name: &'static str,
    /// Features to enable on it.
    pub features: &'static [&'static str],
    /// Whether the crate's default features are wanted.
    pub default_features: bool,
}

impl CrateRequirement {
    /// The `cargo add` invocation that satisfies this requirement.
    pub fn cargo_add_args(&self) -> Vec<String> {
        let mut args = vec!["add".to_owned(), self.name.to_owned()];
        if !self.default_features {
            args.push("--no-default-features".to_owned());
        }
        if !self.features.is_empty() {
            args.push("--features".to_owned());
            args.push(self.features.join(","));
        }
        args
    }

    /// The same invocation as a copy-pasteable command line.
    pub fn cargo_add_command(&self) -> String {
        format!("cargo {}", self.cargo_add_args().join(" "))
    }
}

/// A capability a user can ask for by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Capability {
    /// The name typed on the command line: `skyzen add redis`.
    pub name: &'static str,
    /// One line of explanation, printed by `skyzen add --list`.
    pub summary: &'static str,
    /// The crates it needs.
    pub crates: &'static [CrateRequirement],
}

/// A crate with default features and nothing extra.
const fn plain(name: &'static str) -> CrateRequirement {
    CrateRequirement {
        name,
        features: &[],
        default_features: true,
    }
}

/// A crate with default features off and exactly the listed features on.
const fn only(name: &'static str, features: &'static [&'static str]) -> CrateRequirement {
    CrateRequirement {
        name,
        features,
        default_features: false,
    }
}

const SERVICES: &[CrateRequirement] = &[plain("skyzen-services")];
const TEST: &[CrateRequirement] = &[plain("skyzen-test")];
const REDIS: &[CrateRequirement] = &[plain("skyzen-services"), plain("skyzen-redis")];
const S3: &[CrateRequirement] = &[plain("skyzen-services"), plain("skyzen-s3")];
const SQS: &[CrateRequirement] = &[plain("skyzen-services"), only("skyzen-aws", &["sqs"])];
const DYNAMODB: &[CrateRequirement] =
    &[plain("skyzen-services"), only("skyzen-aws", &["dynamodb"])];
const COSMOS: &[CrateRequirement] = &[plain("skyzen-services"), only("skyzen-azure", &["cosmos"])];
const AZURE_BLOB: &[CrateRequirement] =
    &[plain("skyzen-services"), only("skyzen-azure", &["blob"])];
const SERVICE_BUS: &[CrateRequirement] = &[
    plain("skyzen-services"),
    only("skyzen-azure", &["servicebus"]),
];
const CLOUDFLARE: &[CrateRequirement] = &[plain("skyzen-services"), plain("skyzen-cloudflare")];
const POSTGRES: &[CrateRequirement] = &[CrateRequirement {
    name: "skyzen-services",
    features: &["postgres"],
    default_features: true,
}];
const MYSQL: &[CrateRequirement] = &[CrateRequirement {
    name: "skyzen-services",
    features: &["mysql"],
    default_features: true,
}];
const SQLITE: &[CrateRequirement] = &[CrateRequirement {
    name: "skyzen-services",
    features: &["sqlite"],
    default_features: true,
}];

/// Every capability `skyzen add` understands, in the order `--list` prints them.
pub const CATALOGUE: &[Capability] = &[
    Capability {
        name: "kv",
        summary: "the portable key/value extractor (`Kv`)",
        crates: SERVICES,
    },
    Capability {
        name: "storage",
        summary: "the portable object-storage extractor (`Storage`)",
        crates: SERVICES,
    },
    Capability {
        name: "queue",
        summary: "the portable message-queue extractor (`Queue`)",
        crates: SERVICES,
    },
    Capability {
        name: "db",
        summary: "the portable SQL extractor (`Db`)",
        crates: SERVICES,
    },
    Capability {
        name: "memory",
        summary: "in-process mocks, for `backend = \"memory\"` and tests",
        crates: TEST,
    },
    Capability {
        name: "redis",
        summary: "a Redis-backed KV store (`backend = \"redis\"`)",
        crates: REDIS,
    },
    Capability {
        name: "s3",
        summary: "S3-compatible object storage (`backend = \"s3\"`)",
        crates: S3,
    },
    Capability {
        name: "sqs",
        summary: "an Amazon SQS queue (`backend = \"sqs\"`)",
        crates: SQS,
    },
    Capability {
        name: "dynamodb",
        summary: "a DynamoDB-backed KV store",
        crates: DYNAMODB,
    },
    Capability {
        name: "cosmos",
        summary: "a Cosmos DB-backed KV store",
        crates: COSMOS,
    },
    Capability {
        name: "azure-blob",
        summary: "Azure Blob object storage",
        crates: AZURE_BLOB,
    },
    Capability {
        name: "servicebus",
        summary: "an Azure Service Bus queue",
        crates: SERVICE_BUS,
    },
    Capability {
        name: "cloudflare",
        summary: "Cloudflare KV, R2, Queues, D1 and Durable Objects",
        crates: CLOUDFLARE,
    },
    Capability {
        name: "postgres",
        summary: "the PostgreSQL driver (`backend = \"postgres\"`)",
        crates: POSTGRES,
    },
    Capability {
        name: "mysql",
        summary: "the MySQL driver (`backend = \"mysql\"`)",
        crates: MYSQL,
    },
    Capability {
        name: "sqlite",
        summary: "the SQLite driver (`backend = \"sqlite\"`)",
        crates: SQLITE,
    },
];

/// Look one capability up by the name the user typed.
///
/// # Errors
///
/// Fails when the name is not in the catalogue, listing the names that are.
pub fn lookup(name: &str) -> Result<&'static Capability> {
    CATALOGUE
        .iter()
        .find(|capability| capability.name == name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown capability `{name}`; expected one of: {}",
                CATALOGUE
                    .iter()
                    .map(|capability| capability.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The capability name a `[[service]]` type maps to, for messages.
const fn service_capability(service_type: ServiceType) -> &'static str {
    match service_type {
        ServiceType::Kv => "kv",
        ServiceType::Storage => "storage",
        ServiceType::Queue => "queue",
    }
}

/// The capability a native service backend needs.
const fn native_service_capability(backend: NativeServiceBackend) -> &'static str {
    match backend {
        NativeServiceBackend::Redis => "redis",
        NativeServiceBackend::S3 => "s3",
        NativeServiceBackend::Sqs => "sqs",
        NativeServiceBackend::Memory => "memory",
    }
}

/// The capability a native database backend needs.
const fn native_database_capability(backend: NativeDatabaseBackend) -> &'static str {
    match backend {
        NativeDatabaseBackend::Postgres => "postgres",
        NativeDatabaseBackend::Mysql => "mysql",
        NativeDatabaseBackend::Sqlite => "sqlite",
    }
}

/// Every capability the manifest's declarations imply.
///
/// Both target wirings are included regardless of which provider is being built for: a project
/// that declares a native backend and a Cloudflare binding compiles for both, and the missing
/// crate would surface as a macro-expansion error on whichever target the user tried second.
pub fn required(manifest: &SkyzenManifest) -> Vec<&'static Capability> {
    let mut names = BTreeSet::new();

    for service in &manifest.service {
        names.insert(service_capability(service.service_type));
    }
    if !manifest.database.is_empty() {
        names.insert("db");
    }

    if let Some(native) = &manifest.native {
        for service in native.service.values() {
            names.insert(native_service_capability(service.backend));
        }
        for database in native.database.values() {
            names.insert(native_database_capability(database.backend));
        }
    }

    if manifest
        .cloudflare
        .as_ref()
        .is_some_and(|cloudflare| !cloudflare.service.is_empty() || !cloudflare.database.is_empty())
    {
        names.insert("cloudflare");
    }

    names
        .into_iter()
        .filter_map(|name| CATALOGUE.iter().find(|capability| capability.name == name))
        .collect()
}

/// Fail when the project is missing a crate its manifest declarations need.
///
/// The old CLI silently rewrote `Cargo.toml` here. Reporting instead keeps the project's
/// dependencies something the user owns, and the message is the command that fixes it.
///
/// # Errors
///
/// Fails when a required crate is absent from the project's dependencies.
pub fn ensure_present(manifest: &SkyzenManifest, project: &Project) -> Result<()> {
    let mut missing: Vec<&'static CrateRequirement> = Vec::new();
    for capability in required(manifest) {
        for requirement in capability.crates {
            if !project.depends_on(requirement.name)
                && !missing.iter().any(|held| held.name == requirement.name)
            {
                missing.push(requirement);
            }
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    let commands = missing
        .iter()
        .map(|requirement| format!("  {}", requirement.cargo_add_command()))
        .collect::<Vec<_>>()
        .join("\n");
    let names = missing
        .iter()
        .map(|requirement| requirement.name)
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "Skyzen.toml declares capabilities that need crates {names} is missing from {}.\nRun:\n{commands}",
        project.manifest_path().display()
    )
}

/// Run `skyzen add`.
///
/// # Errors
///
/// Fails when a capability name is unknown, or when a `cargo add` invocation fails.
pub fn add(root: &Path, names: &[String], list_only: bool) -> Result<()> {
    let mut requirements: Vec<&'static CrateRequirement> = Vec::new();
    for name in names {
        for requirement in lookup(name)?.crates {
            if !requirements
                .iter()
                .any(|held| held.name == requirement.name)
            {
                requirements.push(requirement);
            }
        }
    }

    for requirement in requirements {
        let args = requirement.cargo_add_args();
        if list_only {
            output::dry_run(requirement.cargo_add_command());
            continue;
        }

        output::step(requirement.cargo_add_command());
        let status = Command::new("cargo")
            .args(&args)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("failed to launch `{}`", requirement.cargo_add_command()))?;
        if !status.success() {
            anyhow::bail!("`{}` failed", requirement.cargo_add_command());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{lookup, required, CATALOGUE};
    use skyzen_manifest::Manifest;

    fn manifest(source: &str) -> skyzen_manifest::SkyzenManifest {
        Manifest::parse(source, "Skyzen.toml", ".")
            .expect("valid manifest")
            .data()
            .clone()
    }

    #[test]
    fn every_catalogue_name_is_unique_and_resolvable() {
        let mut names: Vec<_> = CATALOGUE.iter().map(|entry| entry.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "capability names must be unique");

        for entry in CATALOGUE {
            assert_eq!(lookup(entry.name).expect("resolvable").name, entry.name);
        }
    }

    #[test]
    fn an_unknown_capability_lists_the_known_ones() {
        let error = lookup("kafka").expect_err("kafka is not a Skyzen capability");
        assert!(error.to_string().contains("redis"), "{error}");
    }

    #[test]
    fn a_manifest_implies_both_its_native_and_its_cloudflare_crates() {
        let manifest = manifest(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [[database]]\nname = \"main\"\ntype = \"sql\"\n\n\
             [native.service.cache]\nbackend = \"redis\"\nurl_env = \"CACHE_URL\"\n\n\
             [native.database.main]\nbackend = \"postgres\"\nurl_env = \"DATABASE_URL\"\n\n\
             [cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.service.cache]\nbinding = \"CACHE\"\n",
        );

        let names: Vec<_> = required(&manifest)
            .into_iter()
            .map(|capability| capability.name)
            .collect();
        assert!(names.contains(&"kv"), "{names:?}");
        assert!(names.contains(&"db"), "{names:?}");
        assert!(names.contains(&"redis"), "{names:?}");
        assert!(names.contains(&"postgres"), "{names:?}");
        assert!(names.contains(&"cloudflare"), "{names:?}");
    }

    #[test]
    fn a_manifest_with_no_capabilities_needs_nothing() {
        let manifest = manifest("[cloudflare]\ncompatibility_date = \"2025-02-01\"\n");
        assert!(required(&manifest).is_empty());
    }

    #[test]
    fn feature_restricted_crates_render_the_flags_cargo_add_needs() {
        let sqs = lookup("sqs").expect("sqs");
        let aws = sqs
            .crates
            .iter()
            .find(|requirement| requirement.name == "skyzen-aws")
            .expect("skyzen-aws");
        assert_eq!(
            aws.cargo_add_command(),
            "cargo add skyzen-aws --no-default-features --features sqs"
        );
    }
}
