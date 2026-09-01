//! The typed model of `Skyzen.toml`.
//!
//! `Skyzen.toml` has two consumers that must never disagree about it: `#[skyzen::main]`, which
//! reads it at compile time to generate service wiring, and the `skyzen` CLI, which reads it to
//! render provider configuration and drive deployments. Historically each had its own parser,
//! and they drifted — a section one accepted, the other rejected. This crate is the single
//! schema both deserialize through, so a section can only ever be understood one way.
//!
//! # Environments
//!
//! `[cloudflare.env.<name>]` declares an overlay with the same shape as `[cloudflare]`.
//! [`Manifest::parse`] resolves every overlay against the base eagerly, so a typo in an
//! environment nobody selected still fails the parse. Overlays compose with [`deep_merge`].
//!
//! # Deploy-time interpolation
//!
//! String values may contain `${NAME}` placeholders. The CLI expands them from the process
//! environment and the project's `.env` files when it reads the file. [`Manifest::parse`] leaves
//! them as written so `#[skyzen::main]` does not depend on secrets the compiler does not have.
//! A missing name fails the CLI parse; there is no default.
//!
//! ```
//! # use skyzen_manifest::Manifest;
//! let manifest = Manifest::parse(
//!     r#"
//!     [cloudflare]
//!     name = "app"
//!     compatibility_date = "2025-02-01"
//!     workers_dev = true
//!
//!     [cloudflare.env.staging]
//!     name = "app-staging"
//!     "#,
//!     "Skyzen.toml",
//!     std::path::PathBuf::from("."),
//! )
//! .unwrap();
//!
//! let base = manifest.cloudflare(None).unwrap().unwrap();
//! assert_eq!(base.name.as_deref(), Some("app"));
//!
//! let staging = manifest.cloudflare(Some("staging")).unwrap().unwrap();
//! assert_eq!(staging.name.as_deref(), Some("app-staging"));
//! // Keys the overlay leaves alone are inherited.
//! assert_eq!(staging.workers_dev, Some(true));
//! ```

mod interpolate;
mod merge;
pub mod migrations;
mod schema;
mod secrets;
mod walk;

pub use interpolate::{expand, expand_table, process_env, EnvLookup, InterpolateError};
pub use merge::deep_merge;
pub use migrations::{MigrationFile, MigrationsError, DEFAULT_MIGRATIONS_DIR};
pub use schema::{
    AwsSection, AzureHttpMode, AzureQueueTrigger, AzureSection, BlobWiring, CfAssets,
    CfAssetsNotFoundHandling, CfD1Database, CfDurableBinding, CfDurableMigration, CfDurableObjects,
    CfDurableRenamedClass, CfHandlers, CfKvNamespace, CfQueueConsumer, CfQueueProducer, CfQueues,
    CfR2Bucket, CfServiceBinding, CfTriggers, CloudflareDatabaseSection, CloudflareSecretSection,
    CloudflareSection, CloudflareServiceSection, CosmosWiring, DatabaseEntry, DatabaseType,
    DynamoDbWiring, InvalidVarName, LambdaArchitecture, MemoryWiring, NativeDatabaseBackend,
    NativeDatabaseSection, NativeQueueConsumer, NativeSection, NativeServiceBackend,
    NativeServiceSection, PartialRdsDataWiring, QueueTriggerError, RdsDataParts, RdsDataWiring,
    RdsEngine, RedisWiring, S3Wiring, SecretEntry, ServiceBusWiring, ServiceEntry, ServiceType,
    SkyzenManifest, SqlUrlWiring, SqsWiring, StorageQueueWiring, VarName, WiringEnvVar,
    HTTP_FUNCTION_NAME, PLAINTEXT_VARIABLE_TABLES,
};
pub use secrets::{scan_table, SecretError, SecretFinding, SecretReport};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// The key under `[cloudflare]` that holds the named environment overlays.
const ENVIRONMENTS_KEY: &str = "env";

/// Everything that can go wrong turning a `Skyzen.toml` file into a [`Manifest`].
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The file could not be read.
    #[error("failed to read {path}: {source}")]
    Read {
        /// The manifest path.
        path: PathBuf,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
    /// The file is not valid TOML.
    #[error("failed to parse {path}: {source}")]
    Syntax {
        /// The manifest path.
        path: PathBuf,
        /// The underlying TOML failure.
        source: Box<toml::de::Error>,
    },
    /// The file is valid TOML but does not match the Skyzen schema.
    #[error("{path}: {section} is invalid: {source}")]
    Schema {
        /// The manifest path.
        path: PathBuf,
        /// Which part of the document failed — the whole file, or one environment overlay.
        section: String,
        /// The underlying deserialization failure.
        source: Box<toml::de::Error>,
    },
    /// `[cloudflare.env]` is present but is not a table of tables.
    #[error("{path}: [cloudflare.{key}] must be a table of named environment overlays")]
    MalformedEnvironments {
        /// The manifest path.
        path: PathBuf,
        /// The offending key, always `env`.
        key: &'static str,
    },
    /// An `[[azure.queue_triggers]]` entry names a function that cannot be used.
    #[error("{path}: [[azure.queue_triggers]] {source}")]
    QueueTrigger {
        /// The manifest path.
        path: PathBuf,
        /// What is wrong with the entry.
        source: schema::QueueTriggerError,
    },
    /// A `[native.database.<name>]` RDS Data API wiring names some of its four values, not all.
    #[error("{path}: [native.database.{database}] {source}")]
    PartialRdsDataWiring {
        /// The manifest path.
        path: PathBuf,
        /// The `[[database]]` whose wiring is half-written.
        database: String,
        /// Which keys are named and which are missing.
        source: schema::PartialRdsDataWiring,
    },
    /// Two `[[secret]]` entries declare the same name.
    #[error("{path}: `[[secret]]` declares `{name}` twice; one secret is one name")]
    DuplicateSecret {
        /// The manifest path.
        path: PathBuf,
        /// The name declared more than once.
        name: String,
    },
    /// A `[cloudflare.secret.<NAME>]` section backs a secret nothing declares.
    #[error(
        "{path}: [{section}.secret.{name}] backs a secret the manifest does not declare; add \
         `[[secret]]` with `name = \"{name}\"`, or remove the section"
    )]
    UnknownSecret {
        /// The manifest path.
        path: PathBuf,
        /// The section the backing was written in — `cloudflare`, or `cloudflare.env.<name>`.
        section: String,
        /// The secret the section names.
        name: String,
    },
    /// A `[[secret]]` name is also a plaintext variable key on some platform.
    #[error(
        "{path}: `{name}` is declared as `[[secret]]` and is also a key of [{table}]; the two \
         would claim the same name on the deployed application, and [{table}] is uploaded in \
         plaintext. Keep the `[[secret]]`, or rename one of them."
    )]
    SecretIsPlaintextVariable {
        /// The manifest path.
        path: PathBuf,
        /// The name declared twice over.
        name: String,
        /// The plaintext table that also holds it.
        table: String,
    },
    /// A `${NAME}` placeholder in a string value could not be expanded.
    #[error("{path}: {source}")]
    Interpolation {
        /// The manifest path.
        path: PathBuf,
        /// What was wrong with the placeholder, including the TOML path.
        source: interpolate::InterpolateError,
    },
    /// A string value is a documented credential form stored in the file.
    #[error("{path}: {source}")]
    CommittedSecret {
        /// The manifest path.
        path: PathBuf,
        /// What was found, without the secret value.
        source: secrets::SecretError,
    },
    /// A named environment was requested that the manifest does not declare.
    #[error(
        "{path} declares no Cloudflare environment named `{name}`{}",
        if .available.is_empty() {
            " (the manifest declares none)".to_owned()
        } else {
            format!(" (available: {})", .available.join(", "))
        }
    )]
    UnknownEnvironment {
        /// The manifest path.
        path: PathBuf,
        /// The environment that was asked for.
        name: String,
        /// The environments the manifest does declare.
        available: Vec<String>,
    },
}

/// A parsed `Skyzen.toml`, with every Cloudflare environment already resolved against the base.
#[derive(Debug, Clone)]
pub struct Manifest {
    path: PathBuf,
    root_dir: PathBuf,
    data: SkyzenManifest,
    environments: BTreeMap<String, CloudflareSection>,
    /// Heuristic secret-shaped values. Empty unless this manifest was loaded with interpolation
    /// (the CLI). Blocking forms have already failed the parse.
    secret_warnings: Vec<SecretFinding>,
}

impl Manifest {
    /// Read and parse the manifest at `path`, leaving `${NAME}` placeholders as written.
    ///
    /// `root_dir` — the project root every relative path in the manifest is resolved against — is
    /// taken to be the manifest's parent directory.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when the file cannot be read, is not valid TOML, does not match
    /// the schema, or has a malformed `[cloudflare.env]` table.
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        Self::load_with(path, None)
    }

    /// Read and parse the manifest at `path`, expanding `${NAME}` through `lookup`.
    ///
    /// `lookup` is `None` for compile-time consumers. The CLI supplies one that reads the process
    /// environment and the project's `.env` files.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when the file cannot be read, a placeholder cannot be expanded,
    /// the file is not valid TOML, does not match the schema, or has a malformed `[cloudflare.env]`
    /// table.
    pub fn load_with(path: &Path, lookup: Option<EnvLookup<'_>>) -> Result<Self, ManifestError> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|source| ManifestError::Read {
                    path: path.to_path_buf(),
                    source,
                })?
                .join(path)
        };
        let root_dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let content = fs::read_to_string(&path).map_err(|source| ManifestError::Read {
            path: path.clone(),
            source,
        })?;
        Self::parse_with(&content, &path, root_dir, lookup)
    }

    /// Parse manifest `content` that came from `path`, rooted at `root_dir`.
    ///
    /// `path` is used only for error messages, so callers holding the content in memory (tests,
    /// or a manifest embedded in a template) can name whatever the user would recognize.
    /// `${NAME}` placeholders are left as written; see [`parse_with`](Self::parse_with).
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when the content is not valid TOML, does not match the schema,
    /// or has a malformed `[cloudflare.env]` table.
    pub fn parse(
        content: &str,
        path: impl AsRef<Path>,
        root_dir: impl Into<PathBuf>,
    ) -> Result<Self, ManifestError> {
        Self::parse_with(content, path, root_dir, None)
    }

    /// Parse `content`, expanding `${NAME}` through `lookup` before the typed schema runs.
    ///
    /// Expansion walks string **values** only, so a secret containing quotes or newlines cannot
    /// break the document. A missing name fails rather than leaving a placeholder in a field the
    /// CLI would treat as a literal id.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when a placeholder cannot be expanded, the content is not valid
    /// TOML, does not match the schema, or has a malformed `[cloudflare.env]` table.
    pub fn parse_with(
        content: &str,
        path: impl AsRef<Path>,
        root_dir: impl Into<PathBuf>,
        lookup: Option<EnvLookup<'_>>,
    ) -> Result<Self, ManifestError> {
        let path = path.as_ref().to_path_buf();
        let mut document: toml::Table =
            toml::from_str(content).map_err(|source| ManifestError::Syntax {
                path: path.clone(),
                source: Box::new(source),
            })?;

        // Scan the file as committed, before interpolation fills in CI secrets. Macros pass
        // `lookup = None` and skip the scan so `cargo build` does not fail on a token the
        // compiler never needed.
        let secret_warnings = if lookup.is_some() {
            let report = secrets::scan_table(&mut document);
            if let Some(source) = secrets::blocking_error(&report) {
                return Err(ManifestError::CommittedSecret {
                    path: path.clone(),
                    source,
                });
            }
            report.warnings
        } else {
            Vec::new()
        };

        if let Some(lookup) = lookup {
            interpolate::expand_table(&mut document, lookup).map_err(|source| {
                ManifestError::Interpolation {
                    path: path.clone(),
                    source,
                }
            })?;
        }

        // Lift the environment overlays out before any typed deserialization: the base and every
        // overlay then go through the *same* `CloudflareSection`, which is what makes
        // `deny_unknown_fields` cover overlays too. It also makes a nested `[...env.a.env.b]` an
        // error for free, since the merged table would carry an `env` key the schema rejects.
        let overlays = take_environment_overlays(&mut document, &path)?;

        let data: SkyzenManifest =
            document
                .clone()
                .try_into()
                .map_err(|source| ManifestError::Schema {
                    path: path.clone(),
                    section: "the manifest".to_owned(),
                    source: Box::new(source),
                })?;

        // Checked here rather than by each consumer: a Functions name is a directory the CLI
        // writes into and a URL path the runtime mounts, so a malformed one has to be caught once,
        // before either of them acts on it.
        if let Some(azure) = &data.azure {
            azure
                .validate_queue_triggers()
                .map_err(|source| ManifestError::QueueTrigger {
                    path: path.clone(),
                    source,
                })?;
        }

        // An RDS Data API wiring names all four of its values or none of them. Checked here, next
        // to the other cross-field rule, so the macro reports it as a compile error and the CLI
        // reports it before generating anything — one rule, both consumers.
        if let Some(native) = &data.native {
            for (name, database) in &native.database {
                let NativeDatabaseSection::RdsData(wiring) = database else {
                    continue;
                };
                wiring
                    .parts()
                    .map_err(|source| ManifestError::PartialRdsDataWiring {
                        path: path.clone(),
                        database: name.clone(),
                        source,
                    })?;
            }
        }

        let base_cloudflare = document
            .get("cloudflare")
            .and_then(toml::Value::as_table)
            .cloned()
            .unwrap_or_default();

        let mut environments = BTreeMap::new();
        for (name, overlay) in overlays {
            let mut merged = base_cloudflare.clone();
            deep_merge(&mut merged, overlay);
            let section: CloudflareSection =
                merged.try_into().map_err(|source| ManifestError::Schema {
                    path: path.clone(),
                    section: format!("[cloudflare.{ENVIRONMENTS_KEY}.{name}]"),
                    source: Box::new(source),
                })?;
            environments.insert(name, section);
        }

        check_secrets(&data, &environments, &path)?;

        Ok(Self {
            path,
            root_dir: root_dir.into(),
            data,
            environments,
            secret_warnings,
        })
    }

    /// The manifest's own path, for error messages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The project root every relative path in the manifest resolves against.
    #[must_use]
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// The manifest's base configuration, with no environment overlay applied.
    #[must_use]
    pub const fn data(&self) -> &SkyzenManifest {
        &self.data
    }

    /// The names of every declared `[cloudflare.env.<name>]` overlay, in sorted order.
    pub fn environment_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.environments.keys().map(String::as_str)
    }

    /// Heuristic secret-shaped values found while loading with interpolation.
    ///
    /// Blocking credential forms have already failed the parse. These leftovers — a
    /// secret-named key whose value is not a known token, a JWT-shaped string — are
    /// warnings for the CLI to print.
    #[must_use]
    pub fn secret_warnings(&self) -> &[SecretFinding] {
        &self.secret_warnings
    }

    /// The Cloudflare configuration for `environment`, or the base configuration when `None`.
    ///
    /// `Ok(None)` means the manifest has no `[cloudflare]` section at all.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::UnknownEnvironment`] when a named environment was requested that
    /// the manifest does not declare — a silent fall back to the base configuration would deploy
    /// to production while the user believed they had asked for staging.
    pub fn cloudflare(
        &self,
        environment: Option<&str>,
    ) -> Result<Option<&CloudflareSection>, ManifestError> {
        environment.map_or_else(
            || Ok(self.data.cloudflare.as_ref()),
            |name| {
                self.environments.get(name).map(Some).ok_or_else(|| {
                    ManifestError::UnknownEnvironment {
                        path: self.path.clone(),
                        name: name.to_owned(),
                        available: self.environments.keys().cloned().collect(),
                    }
                })
            },
        )
    }
}

/// Check `[[secret]]` against everything that would claim one of its names.
///
/// Three rules, checked once for both consumers: a name is declared at most once, every
/// `[cloudflare.secret.<NAME>]` backs a declared secret, and a secret's name is not also a key of
/// a plaintext variable table — where the value would be uploaded in the clear, under the name the
/// application reads its secret from. Every resolved overlay is checked, not only the base, so an
/// environment that adds the collision fails the parse the same way.
fn check_secrets(
    data: &SkyzenManifest,
    environments: &BTreeMap<String, CloudflareSection>,
    path: &Path,
) -> Result<(), ManifestError> {
    let mut declared: BTreeSet<&str> = BTreeSet::new();
    for entry in &data.secret {
        if !declared.insert(entry.name.as_str()) {
            return Err(ManifestError::DuplicateSecret {
                path: path.to_path_buf(),
                name: entry.name.to_string(),
            });
        }
    }

    if let Some(aws) = &data.aws {
        check_plaintext_table(aws.env.keys(), &declared, path, schema::AWS_ENV_TABLE)?;
    }
    if let Some(cloudflare) = &data.cloudflare {
        check_cloudflare_secrets(cloudflare, &declared, path, "cloudflare")?;
    }
    for (name, section) in environments {
        check_cloudflare_secrets(
            section,
            &declared,
            path,
            &format!("cloudflare.{ENVIRONMENTS_KEY}.{name}"),
        )?;
    }
    Ok(())
}

/// Check one `[cloudflare]` section — the base, or a resolved overlay — against `declared`.
fn check_cloudflare_secrets(
    section: &CloudflareSection,
    declared: &BTreeSet<&str>,
    path: &Path,
    label: &str,
) -> Result<(), ManifestError> {
    for name in section.secret.keys() {
        if !declared.contains(name.as_str()) {
            return Err(ManifestError::UnknownSecret {
                path: path.to_path_buf(),
                section: label.to_owned(),
                name: name.to_string(),
            });
        }
    }
    check_plaintext_table(
        section.vars.keys(),
        declared,
        path,
        &format!("{label}.vars"),
    )
}

/// Fail when a plaintext variable table holds a key a `[[secret]]` already declares.
fn check_plaintext_table<'a>(
    keys: impl Iterator<Item = &'a String>,
    declared: &BTreeSet<&str>,
    path: &Path,
    table: &str,
) -> Result<(), ManifestError> {
    for key in keys {
        if declared.contains(key.as_str()) {
            return Err(ManifestError::SecretIsPlaintextVariable {
                path: path.to_path_buf(),
                name: key.clone(),
                table: table.to_owned(),
            });
        }
    }
    Ok(())
}

/// Remove `[cloudflare.env]` from `document` and return the overlays it held.
fn take_environment_overlays(
    document: &mut toml::Table,
    path: &Path,
) -> Result<BTreeMap<String, toml::Table>, ManifestError> {
    let Some(cloudflare) = document
        .get_mut("cloudflare")
        .and_then(toml::Value::as_table_mut)
    else {
        return Ok(BTreeMap::new());
    };
    let Some(raw) = cloudflare.remove(ENVIRONMENTS_KEY) else {
        return Ok(BTreeMap::new());
    };
    let toml::Value::Table(entries) = raw else {
        return Err(ManifestError::MalformedEnvironments {
            path: path.to_path_buf(),
            key: ENVIRONMENTS_KEY,
        });
    };

    entries
        .into_iter()
        .map(|(name, value)| match value {
            toml::Value::Table(overlay) => Ok((name, overlay)),
            _ => Err(ManifestError::MalformedEnvironments {
                path: path.to_path_buf(),
                key: ENVIRONMENTS_KEY,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        InterpolateError, Manifest, ManifestError, NativeDatabaseBackend, NativeDatabaseSection,
        NativeServiceBackend, NativeServiceSection, RdsEngine, ServiceType, VarName,
    };
    use std::time::Duration;

    fn parse(content: &str) -> Result<Manifest, ManifestError> {
        Manifest::parse(content, "Skyzen.toml", ".")
    }

    fn parse_expanded(content: &str, env: &[(&str, &str)]) -> Result<Manifest, ManifestError> {
        let map: std::collections::BTreeMap<&str, &str> = env.iter().copied().collect();
        let lookup = |name: &str| -> Result<Option<String>, InterpolateError> {
            Ok(map.get(name).map(|value| (*value).to_owned()))
        };
        Manifest::parse_with(content, "Skyzen.toml", ".", Some(&lookup))
    }

    /// The `[native.service.cache]` a manifest wiring `cache` with `body` produces.
    fn service_wiring(body: &str) -> Result<NativeServiceSection, ManifestError> {
        Ok(parse(&format!(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n[native.service.cache]\n{body}"
        ))?
        .data()
        .native
        .as_ref()
        .expect("native")
        .service["cache"]
            .clone())
    }

    /// The `[native.database.main]` a manifest wiring `main` with `body` produces.
    fn database_wiring(body: &str) -> Result<NativeDatabaseSection, ManifestError> {
        Ok(parse(&format!(
            "[[database]]\nname = \"main\"\ntype = \"sql\"\n\n[native.database.main]\n{body}"
        ))?
        .data()
        .native
        .as_ref()
        .expect("native")
        .database["main"]
            .clone())
    }

    #[test]
    fn parses_portable_capabilities_into_typed_discriminants() {
        let manifest = parse(
            "[[service]]\nname = \"cache\"\ntype = \"kv\"\n\n\
             [native.service.cache]\nbackend = \"memory\"\n\n\
             [[database]]\nname = \"main\"\ntype = \"sql\"\ndefault = true\n",
        )
        .expect("manifest parses");

        let data = manifest.data();
        assert_eq!(data.service[0].service_type, ServiceType::Kv);
        assert_eq!(
            data.native.as_ref().expect("native").service["cache"].backend(),
            NativeServiceBackend::Memory
        );
        assert!(data.database[0].default);
    }

    #[test]
    fn a_redis_wiring_names_the_variable_holding_its_url() {
        let NativeServiceSection::Redis(wiring) =
            service_wiring("backend = \"redis\"\nurl_env = \"CACHE_URL\"\n").expect("parses")
        else {
            panic!("expected the redis variant");
        };
        assert_eq!(wiring.url_env, "CACHE_URL");
    }

    #[test]
    fn a_dynamodb_wiring_names_its_table_and_leaves_the_rest_to_the_backend() {
        let NativeServiceSection::DynamoDb(wiring) =
            service_wiring("backend = \"dynamodb\"\ntable = \"skyzen-sessions\"\n")
                .expect("parses")
        else {
            panic!("expected the dynamodb variant");
        };
        assert_eq!(wiring.table, "skyzen-sessions");
        assert_eq!(wiring.ttl_attribute, None);
        assert_eq!(wiring.consistent_reads, None);
    }

    #[test]
    fn a_dynamodb_wiring_carries_the_two_options_the_builder_takes() {
        let NativeServiceSection::DynamoDb(wiring) = service_wiring(
            "backend = \"dynamodb\"\ntable = \"skyzen-sessions\"\n\
             ttl_attribute = \"ttl\"\nconsistent_reads = true\n",
        )
        .expect("parses") else {
            panic!("expected the dynamodb variant");
        };
        assert_eq!(wiring.ttl_attribute.as_deref(), Some("ttl"));
        assert_eq!(wiring.consistent_reads, Some(true));
    }

    #[test]
    fn a_cosmos_wiring_names_its_container_and_reads_the_sdks_own_variables() {
        let wiring = service_wiring(
            "backend = \"cosmos\"\ndatabase = \"appdb\"\ncontainer = \"sessions\"\n",
        )
        .expect("parses");
        let NativeServiceSection::Cosmos(cosmos) = &wiring else {
            panic!("expected the cosmos variant");
        };
        assert_eq!(cosmos.database, "appdb");
        assert_eq!(cosmos.container, "sessions");

        // The endpoint and the key are fixed by `CosmosKv::from_env`, so they are reported as
        // coming from the backend rather than from a key the manifest could have named.
        let names: Vec<_> = wiring.env_vars().iter().map(|var| var.name).collect();
        assert_eq!(names, ["AZURE_COSMOS_ENDPOINT", "AZURE_COSMOS_KEY"]);
        assert!(wiring.env_vars().iter().all(|var| var.key.is_none()));
    }

    #[test]
    fn a_blob_wiring_defaults_to_the_variable_the_azure_sdk_documents() {
        let NativeServiceSection::Blob(wiring) =
            service_wiring("backend = \"blob\"\ncontainer = \"uploads\"\n").expect("parses")
        else {
            panic!("expected the blob variant");
        };
        assert_eq!(wiring.container, "uploads");
        assert_eq!(wiring.connection_env, "AZURE_STORAGE_CONNECTION_STRING");

        let NativeServiceSection::Blob(overridden) = service_wiring(
            "backend = \"blob\"\ncontainer = \"uploads\"\nconnection_env = \"UPLOADS_ACCOUNT\"\n",
        )
        .expect("parses") else {
            panic!("expected the blob variant");
        };
        assert_eq!(overridden.connection_env, "UPLOADS_ACCOUNT");
    }

    #[test]
    fn a_service_bus_wiring_defaults_to_the_variable_the_azure_sdk_documents() {
        let NativeServiceSection::ServiceBus(wiring) =
            service_wiring("backend = \"servicebus\"\nqueue = \"jobs\"\n").expect("parses")
        else {
            panic!("expected the servicebus variant");
        };
        assert_eq!(wiring.queue, "jobs");
        assert_eq!(wiring.connection_env, "SERVICEBUS_CONNECTION_STRING");
    }

    #[test]
    fn a_storage_queue_wiring_names_the_variable_holding_its_signed_url() {
        let wiring =
            service_wiring("backend = \"storage-queue\"\nsas_url_env = \"JOBS_SAS_URL\"\n")
                .expect("parses");
        let NativeServiceSection::StorageQueue(storage_queue) = &wiring else {
            panic!("expected the storage-queue variant");
        };
        assert_eq!(storage_queue.sas_url_env, "JOBS_SAS_URL");
        assert_eq!(wiring.env_vars()[0].key, Some("sas_url_env"));
    }

    #[test]
    fn an_azure_sql_wiring_names_the_variable_holding_its_connection_string() {
        let wiring =
            database_wiring("backend = \"azure-sql\"\nurl_env = \"AZURE_SQL_CONNECTION_STRING\"\n")
                .expect("parses");
        assert_eq!(wiring.backend(), NativeDatabaseBackend::AzureSql);

        // An ADO.NET connection string is not a URL sqlx can dial, so the native migrate runner is
        // told there is no connection here for it to open …
        assert_eq!(wiring.url_env(), None);
        // … while the variable itself is still declared, so `skyzen dev` and `.env.example` cover
        // it the way they cover every other named one.
        assert_eq!(wiring.env_vars()[0].name, "AZURE_SQL_CONNECTION_STRING");
        assert_eq!(wiring.env_vars()[0].key, Some("url_env"));
    }

    #[test]
    fn an_azure_sql_wiring_rejects_a_key_that_belongs_to_another_backend() {
        for (body, unknown) in [
            (
                "backend = \"azure-sql\"\nurl_env = \"C\"\ncontainer = \"uploads\"",
                "container",
            ),
            (
                "backend = \"azure-sql\"\nconnection_env = \"AZURE_SQL_CONNECTION_STRING\"",
                "connection_env",
            ),
        ] {
            let error = database_wiring(&format!("{body}\n"))
                .expect_err("a key azure-sql does not take should be rejected")
                .to_string();
            assert!(error.contains(unknown), "{body}: {error}");
        }
    }

    #[test]
    fn an_rds_data_wiring_that_names_nothing_declares_the_four_variables_instead() {
        let wiring = database_wiring("backend = \"rds-data\"\n").expect("parses");
        assert_eq!(wiring.backend(), NativeDatabaseBackend::RdsData);
        assert_eq!(wiring.url_env(), None);

        let NativeDatabaseSection::RdsData(rds) = &wiring else {
            panic!("expected the rds-data variant");
        };
        assert_eq!(rds.parts().expect("naming none of them is complete"), None);

        let names: Vec<_> = wiring.env_vars().iter().map(|var| var.name).collect();
        assert_eq!(
            names,
            [
                "RDS_RESOURCE_ARN",
                "RDS_SECRET_ARN",
                "RDS_DATABASE",
                "RDS_ENGINE"
            ]
        );
    }

    #[test]
    fn an_rds_data_wiring_that_names_all_four_values_reads_no_variables() {
        let wiring = database_wiring(
            "backend = \"rds-data\"\n\
             resource_arn = \"arn:aws:rds:us-east-1:111122223333:cluster:skyzen\"\n\
             secret_arn = \"arn:aws:secretsmanager:us-east-1:111122223333:secret:skyzen-Ab12Cd\"\n\
             database = \"appdb\"\nengine = \"aurora-postgresql\"\n",
        )
        .expect("parses");

        let NativeDatabaseSection::RdsData(rds) = &wiring else {
            panic!("expected the rds-data variant");
        };
        let parts = rds
            .parts()
            .expect("all four are named")
            .expect("all four are named");
        assert_eq!(
            parts.resource_arn,
            "arn:aws:rds:us-east-1:111122223333:cluster:skyzen"
        );
        assert_eq!(parts.database, "appdb");
        assert_eq!(parts.engine, RdsEngine::AuroraPostgres);
        assert_eq!(parts.engine.as_str(), "aurora-postgresql");

        // The wiring replaced the variables, so `skyzen dev` must not demand them.
        assert!(wiring.env_vars().is_empty(), "{:?}", wiring.env_vars());
    }

    #[test]
    fn an_rds_data_wiring_that_names_only_some_values_is_rejected_naming_the_missing_keys() {
        let error = database_wiring(
            "backend = \"rds-data\"\nresource_arn = \"arn:aws:rds:us-east-1:1:cluster:c\"\n\
             database = \"appdb\"\n",
        )
        .expect_err("half a wiring is not a wiring")
        .to_string();

        assert!(error.contains("[native.database.main]"), "{error}");
        // The keys it does name, and the ones to add.
        assert!(error.contains("`resource_arn`, `database`"), "{error}");
        assert!(error.contains("`secret_arn`, `engine`"), "{error}");
    }

    #[test]
    fn an_rds_data_wiring_rejects_an_engine_rds_does_not_name() {
        let error = database_wiring(
            "backend = \"rds-data\"\nresource_arn = \"a\"\nsecret_arn = \"b\"\n\
             database = \"appdb\"\nengine = \"postgres\"\n",
        )
        .expect_err("`postgres` is not an RDS engine identifier")
        .to_string();
        assert!(error.contains("aurora-postgresql"), "{error}");
    }

    #[test]
    fn every_backend_spelling_parses_back_to_its_own_discriminant() {
        // `as_str` is what the CLI and the docs print, and the `#[serde(rename)]` on the section is
        // what the parser accepts. A test rather than a comment, because the two are written twice.
        let body = |backend: NativeServiceBackend| match backend {
            NativeServiceBackend::Redis | NativeServiceBackend::Sqs => "url_env = \"X\"".to_owned(),
            NativeServiceBackend::S3 => "bucket_env = \"X\"".to_owned(),
            NativeServiceBackend::DynamoDb => "table = \"x\"".to_owned(),
            NativeServiceBackend::Cosmos => "database = \"a\"\ncontainer = \"b\"".to_owned(),
            NativeServiceBackend::Blob => "container = \"b\"".to_owned(),
            NativeServiceBackend::ServiceBus => "queue = \"q\"".to_owned(),
            NativeServiceBackend::StorageQueue => "sas_url_env = \"X\"".to_owned(),
            NativeServiceBackend::Memory => String::new(),
        };

        for backend in NativeServiceBackend::ALL {
            let wiring = service_wiring(&format!(
                "backend = \"{}\"\n{}\n",
                backend.as_str(),
                body(backend)
            ))
            .unwrap_or_else(|error| panic!("{} should parse: {error}", backend.as_str()));
            assert_eq!(wiring.backend(), backend);
        }

        for backend in NativeDatabaseBackend::ALL {
            let body = if backend == NativeDatabaseBackend::RdsData {
                String::new()
            } else {
                "url_env = \"DATABASE_URL\"".to_owned()
            };
            let wiring = database_wiring(&format!("backend = \"{}\"\n{body}\n", backend.as_str()))
                .unwrap_or_else(|error| panic!("{} should parse: {error}", backend.as_str()));
            assert_eq!(wiring.backend(), backend);
        }
    }

    #[test]
    fn a_key_that_belongs_to_another_backend_is_rejected_where_it_is_written() {
        // One per variant: the whole point of the tagged shape is that `url_env` under a backend
        // that reads no URL is a mistake the parser catches, not a key silently ignored.
        for (body, unknown) in [
            (
                "backend = \"redis\"\nbucket_env = \"B\"\nurl_env = \"U\"",
                "bucket_env",
            ),
            (
                "backend = \"dynamodb\"\ntable = \"t\"\nurl_env = \"U\"",
                "url_env",
            ),
            (
                "backend = \"cosmos\"\ndatabase = \"a\"\ncontainer = \"b\"\nendpoint_env = \"E\"",
                "endpoint_env",
            ),
            (
                "backend = \"s3\"\nbucket_env = \"B\"\nurl_env = \"U\"",
                "url_env",
            ),
            (
                "backend = \"blob\"\ncontainer = \"c\"\nbucket_env = \"B\"",
                "bucket_env",
            ),
            ("backend = \"sqs\"\nurl_env = \"U\"\nqueue = \"q\"", "queue"),
            (
                "backend = \"servicebus\"\nqueue = \"q\"\nurl_env = \"U\"",
                "url_env",
            ),
            (
                "backend = \"storage-queue\"\nsas_url_env = \"U\"\nqueue = \"q\"",
                "queue",
            ),
            ("backend = \"memory\"\nurl_env = \"U\"", "url_env"),
        ] {
            let error = service_wiring(&format!("{body}\n"))
                .expect_err("a key from another backend should be rejected")
                .to_string();
            assert!(error.contains(unknown), "{body}: {error}");
        }
    }

    #[test]
    fn a_missing_required_key_fails_the_parse_rather_than_the_build() {
        for (body, key) in [
            ("backend = \"redis\"", "url_env"),
            ("backend = \"dynamodb\"", "table"),
            ("backend = \"cosmos\"\ndatabase = \"a\"", "container"),
            ("backend = \"s3\"", "bucket_env"),
            ("backend = \"blob\"", "container"),
            ("backend = \"sqs\"", "url_env"),
            ("backend = \"servicebus\"", "queue"),
            ("backend = \"storage-queue\"", "sas_url_env"),
        ] {
            let error = service_wiring(&format!("{body}\n"))
                .expect_err("a required key is missing")
                .to_string();
            assert!(error.contains(key), "{body}: {error}");
        }
    }

    #[test]
    fn an_rds_data_wiring_rejects_a_key_that_names_a_variable_rather_than_a_value() {
        // The four values are named directly (`resource_arn = "arn:…"`); the variables the
        // fall-back constructor reads are fixed by it, so there is no `*_env` key to point at one.
        for key in ["resource_arn_env", "engine_env", "url_env"] {
            let error = database_wiring(&format!("backend = \"rds-data\"\n{key} = \"x\"\n"))
                .expect_err("the RDS Data API takes values, not variable names")
                .to_string();
            assert!(error.contains(key), "{key}: {error}");
        }
    }

    #[test]
    fn an_unknown_backend_names_the_value_it_rejected() {
        let error = service_wiring("backend = \"kafka\"\n").expect_err("not a backend");
        assert!(error.to_string().contains("kafka"), "{error}");
    }

    #[test]
    fn a_queue_consumer_entry_fills_in_the_polling_defaults() {
        let manifest = parse(
            "[[service]]\nname = \"jobs\"\ntype = \"queue\"\n\n\
             [native.service.jobs]\nbackend = \"memory\"\n\n\
             [[native.queue_consumer]]\nservice = \"jobs\"\n",
        )
        .expect("manifest parses");

        let consumer = &manifest
            .data()
            .native
            .as_ref()
            .expect("native")
            .queue_consumer[0];
        assert_eq!(consumer.service, "jobs");
        assert_eq!(consumer.concurrency.get(), 1);
        assert_eq!(consumer.batch_size.get(), 10);
        assert_eq!(consumer.poll_wait, Duration::from_secs(20));
        assert_eq!(consumer.visibility_timeout, None);
        assert_eq!(consumer.retry_delay, Duration::from_secs(30));
    }

    #[test]
    fn a_queue_consumer_entry_parses_its_humantime_durations() {
        let manifest = parse(
            "[[service]]\nname = \"jobs\"\ntype = \"queue\"\n\n\
             [[native.queue_consumer]]\nservice = \"jobs\"\nconcurrency = 4\nbatch_size = 5\n\
             poll_wait = \"1s\"\nvisibility_timeout = \"1m 30s\"\nretry_delay = \"250ms\"\n",
        )
        .expect("manifest parses");

        let consumer = &manifest
            .data()
            .native
            .as_ref()
            .expect("native")
            .queue_consumer[0];
        assert_eq!(consumer.concurrency.get(), 4);
        assert_eq!(consumer.batch_size.get(), 5);
        assert_eq!(consumer.poll_wait, Duration::from_secs(1));
        assert_eq!(consumer.visibility_timeout, Some(Duration::from_secs(90)));
        assert_eq!(consumer.retry_delay, Duration::from_millis(250));
    }

    #[test]
    fn rejects_a_malformed_consumer_duration_at_parse_time() {
        let error = parse(
            "[[native.queue_consumer]]\nservice = \"jobs\"\npoll_wait = \"twenty seconds\"\n",
        )
        .expect_err("bad duration");
        assert!(
            error.to_string().contains("poll_wait"),
            "error should name the rejected key: {error}"
        );
    }

    #[test]
    fn rejects_a_zero_consumer_concurrency() {
        let error = parse("[[native.queue_consumer]]\nservice = \"jobs\"\nconcurrency = 0\n")
            .expect_err("zero concurrency");
        assert!(
            error.to_string().contains("concurrency"),
            "error should name the rejected key: {error}"
        );
    }

    #[test]
    fn the_aws_section_defaults_to_arm64_with_a_function_url() {
        let manifest = parse("[aws]\nfunction_name = \"api\"\n").expect("manifest parses");

        let aws = manifest.data().aws.as_ref().expect("aws section");
        assert_eq!(aws.function_name.as_deref(), Some("api"));
        assert_eq!(aws.architecture, super::LambdaArchitecture::Arm64);
        assert_eq!(
            aws.architecture.target_triple(),
            "aarch64-unknown-linux-gnu"
        );
        assert!(aws.url, "an HTTP application is unreachable without one");
        assert_eq!(aws.memory_mb, None);
        assert_eq!(aws.timeout, None);
        assert!(aws.env.is_empty());
    }

    #[test]
    fn the_aws_section_parses_its_sizing_and_environment() {
        let manifest = parse(
            "[aws]\nmemory_mb = 512\ntimeout = \"30s\"\narchitecture = \"x86_64\"\nurl = false\n\n\
             [aws.env]\nRUST_LOG = \"info\"\n",
        )
        .expect("manifest parses");

        let aws = manifest.data().aws.as_ref().expect("aws section");
        assert_eq!(aws.memory_mb.map(std::num::NonZeroU32::get), Some(512));
        assert_eq!(aws.timeout, Some(Duration::from_secs(30)));
        assert_eq!(aws.architecture.target_triple(), "x86_64-unknown-linux-gnu");
        assert!(!aws.url);
        assert_eq!(aws.env["RUST_LOG"], "info");
    }

    #[test]
    fn rejects_a_zero_lambda_memory_size_and_an_unknown_architecture() {
        let zero = parse("[aws]\nmemory_mb = 0\n").expect_err("zero memory");
        assert!(zero.to_string().contains("memory_mb"), "{zero}");

        let architecture =
            parse("[aws]\narchitecture = \"riscv\"\n").expect_err("unknown architecture");
        assert!(architecture.to_string().contains("riscv"), "{architecture}");
    }

    #[test]
    fn the_azure_section_parses_its_queue_triggers() {
        let manifest = parse(
            "[azure]\napp_name = \"skyzen-demo\"\ntarget = \"x86_64-unknown-linux-musl\"\n\
             http_mode = \"proxy\"\n\n\
             [[azure.queue_triggers]]\nfunction = \"process\"\nqueue = \"jobs\"\n\
             connection_env = \"AzureWebJobsStorage\"\n",
        )
        .expect("manifest parses");

        let azure = manifest.data().azure.as_ref().expect("azure section");
        assert_eq!(azure.app_name.as_deref(), Some("skyzen-demo"));
        assert_eq!(azure.target.as_deref(), Some("x86_64-unknown-linux-musl"));
        assert_eq!(azure.http_mode.host_json_key(), "enableProxyingHttpRequest");
        assert_eq!(azure.queue_triggers[0].function, "process");
        assert_eq!(azure.queue_triggers[0].queue, "jobs");
        assert_eq!(
            azure.queue_triggers[0].connection_env,
            "AzureWebJobsStorage"
        );
    }

    #[test]
    fn the_azure_section_forwards_http_by_default() {
        let manifest = parse("[azure]\n").expect("manifest parses");

        let azure = manifest.data().azure.as_ref().expect("azure section");
        assert_eq!(
            azure.http_mode.host_json_key(),
            "enableForwardingHttpRequest"
        );
        assert!(
            azure.queue_triggers.is_empty(),
            "{:?}",
            azure.queue_triggers
        );
    }

    fn trigger(function: &str) -> Result<Manifest, ManifestError> {
        parse(&format!(
            "[[azure.queue_triggers]]\nfunction = \"{function}\"\nqueue = \"jobs\"\n\
             connection_env = \"AzureWebJobsStorage\"\n"
        ))
    }

    #[test]
    fn a_queue_trigger_cannot_take_the_catch_all_http_functions_name() {
        // It would replace the one function that serves every route, and the deployment would come
        // up answering nothing.
        for name in ["http", "HTTP", "Http"] {
            let error = trigger(name).expect_err("the name is reserved");
            assert!(error.to_string().contains("catch-all"), "{name}: {error}");
        }
    }

    #[test]
    fn a_queue_trigger_name_that_is_a_path_is_refused() {
        // `function` becomes a directory the CLI writes into, so a traversal must never reach it.
        for name in ["../escape", "a/b", "", "9lives", "with space"] {
            let error = trigger(name).expect_err("not a Functions name");
            assert!(
                error.to_string().contains("valid Functions name"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn a_valid_queue_trigger_name_is_accepted() {
        for name in ["process", "process-jobs", "process_jobs", "p9"] {
            trigger(name).unwrap_or_else(|error| panic!("{name} should parse: {error}"));
        }
    }

    #[test]
    fn two_queue_triggers_cannot_share_a_name() {
        let error = parse(
            "[[azure.queue_triggers]]\nfunction = \"process\"\nqueue = \"a\"\n\
             connection_env = \"AzureWebJobsStorage\"\n\n\
             [[azure.queue_triggers]]\nfunction = \"Process\"\nqueue = \"b\"\n\
             connection_env = \"AzureWebJobsStorage\"\n",
        )
        .expect_err("one name, one directory, one URL path");
        assert!(error.to_string().contains("twice"), "{error}");
    }

    #[test]
    fn a_queue_trigger_must_name_its_connection_setting() {
        let error = parse("[[azure.queue_triggers]]\nfunction = \"process\"\nqueue = \"jobs\"\n")
            .expect_err("no connection_env");
        assert!(error.to_string().contains("connection_env"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_top_level_section() {
        let error = parse("[[datasource]]\nname = \"MainDb\"\n").expect_err("unknown section");
        assert!(
            error.to_string().contains("datasource"),
            "error should name the rejected key: {error}"
        );
    }

    #[test]
    fn rejects_an_unknown_key_inside_the_cloudflare_section() {
        let error = parse("[cloudflare]\nworkers_devv = true\n").expect_err("unknown key");
        assert!(
            error.to_string().contains("workers_devv"),
            "error should name the rejected key: {error}"
        );
    }

    #[test]
    fn rejects_an_unsupported_service_type() {
        let error = parse("[[service]]\nname = \"x\"\ntype = \"graph\"\n").expect_err("bad type");
        assert!(
            error.to_string().contains("graph"),
            "error should name the rejected value: {error}"
        );
    }

    #[test]
    fn validates_every_environment_overlay_not_only_the_selected_one() {
        let error = parse(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.env.staging]\nworkers_devv = true\n",
        )
        .expect_err("overlay typo");
        assert!(
            error.to_string().contains("staging") && error.to_string().contains("workers_devv"),
            "error should locate the overlay and the key: {error}"
        );
    }

    #[test]
    fn rejects_a_nested_environment_overlay() {
        let error = parse(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.env.staging.env.deeper]\nname = \"no\"\n",
        )
        .expect_err("nested env");
        assert!(
            error.to_string().contains("env"),
            "error should name the rejected key: {error}"
        );
    }

    #[test]
    fn an_overlay_inherits_and_overrides_the_base() {
        let manifest = parse(
            "[cloudflare]\nname = \"app\"\ncompatibility_date = \"2025-02-01\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"base-id\"\n\n\
             [cloudflare.env.staging]\nname = \"app-staging\"\n\n\
             [[cloudflare.env.staging.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"staging-id\"\n",
        )
        .expect("manifest parses");

        let staging = manifest
            .cloudflare(Some("staging"))
            .expect("known environment")
            .expect("cloudflare section");
        assert_eq!(staging.name.as_deref(), Some("app-staging"));
        assert_eq!(staging.compatibility_date.as_deref(), Some("2025-02-01"));
        // Arrays replace wholesale, so the overlay must restate the whole binding list.
        assert_eq!(staging.kv_namespaces.len(), 1);
        assert_eq!(staging.kv_namespaces[0].id.as_deref(), Some("staging-id"));
    }

    #[test]
    fn selecting_an_undeclared_environment_is_an_error_not_a_fall_back() {
        let manifest = parse(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.env.staging]\nname = \"app-staging\"\n",
        )
        .expect("manifest parses");

        let error = manifest
            .cloudflare(Some("prod"))
            .expect_err("unknown environment");
        assert!(
            error.to_string().contains("prod") && error.to_string().contains("staging"),
            "error should name the request and the alternatives: {error}"
        );
    }

    #[test]
    fn interpolates_string_values_from_the_supplied_lookup() {
        let manifest = parse_expanded(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\naccount_id = \"${ACCOUNT}\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"${CACHE_ID}\"\n\n\
             [cloudflare.vars]\nAPI_URL = \"${API_URL}\"\n\n\
             [cloudflare.env.staging]\naccount_id = \"${STAGING_ACCOUNT}\"\n",
            &[
                ("ACCOUNT", "acct_prod"),
                ("CACHE_ID", "ns_abc"),
                ("API_URL", "https://api.flyco.io"),
                ("STAGING_ACCOUNT", "acct_staging"),
            ],
        )
        .expect("manifest parses");

        let base = manifest
            .cloudflare(None)
            .expect("base")
            .expect("cloudflare");
        assert_eq!(base.account_id.as_deref(), Some("acct_prod"));
        assert_eq!(base.kv_namespaces[0].id.as_deref(), Some("ns_abc"));
        assert_eq!(base.vars["API_URL"], "https://api.flyco.io");

        let staging = manifest
            .cloudflare(Some("staging"))
            .expect("known environment")
            .expect("cloudflare");
        assert_eq!(staging.account_id.as_deref(), Some("acct_staging"));
        // Overlay inherits the expanded base binding list.
        assert_eq!(staging.kv_namespaces[0].id.as_deref(), Some("ns_abc"));
    }

    #[test]
    fn parse_without_expansion_leaves_placeholders_in_place() {
        let manifest = parse(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"${CACHE_ID}\"\n",
        )
        .expect("literal parse");
        assert_eq!(
            manifest
                .data()
                .cloudflare
                .as_ref()
                .expect("cloudflare")
                .kv_namespaces[0]
                .id
                .as_deref(),
            Some("${CACHE_ID}")
        );
    }

    #[test]
    fn a_missing_interpolation_fails_naming_the_variable_and_the_key() {
        let error = parse_expanded(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\naccount_id = \"${ACCOUNT}\"\n",
            &[],
        )
        .expect_err("unset");
        let rendered = error.to_string();
        assert!(rendered.contains("ACCOUNT"), "{rendered}");
        assert!(rendered.contains("account_id"), "{rendered}");
        assert!(rendered.contains("Skyzen.toml"), "{rendered}");
    }

    #[test]
    fn interpolating_a_value_with_quotes_does_not_break_toml() {
        let manifest = parse_expanded(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.vars]\nBANNER = \"${BANNER}\"\n",
            &[("BANNER", "he said \"hi\"")],
        )
        .expect("manifest parses");
        assert_eq!(
            manifest
                .data()
                .cloudflare
                .as_ref()
                .expect("cloudflare")
                .vars["BANNER"],
            "he said \"hi\""
        );
    }

    #[test]
    fn the_cli_parse_blocks_a_github_token_and_the_macro_parse_does_not() {
        let source = "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.vars]\nTOKEN = \"ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n";
        let error = parse_expanded(source, &[]).expect_err("CLI load blocks a known token");
        let rendered = error.to_string();
        assert!(
            rendered.contains("GitHub personal access token"),
            "{rendered}"
        );
        assert!(!rendered.contains("ghp_"), "{rendered}");

        parse(source).expect("compile-time parse does not scan");
    }

    #[test]
    fn a_secret_named_literal_warns_on_cli_load_but_does_not_fail() {
        let manifest = parse_expanded(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.vars]\nAPI_KEY = \"dev-only\"\n",
            &[],
        )
        .expect("heuristic is a warning");
        assert_eq!(manifest.secret_warnings().len(), 1);
        assert!(
            manifest.secret_warnings()[0]
                .kind
                .starts_with("plaintext value of a secret-named key"),
            "{:?}",
            manifest.secret_warnings()[0]
        );
        // The fix the warning names is the portable one.
        assert!(
            manifest.secret_warnings()[0].kind.contains("[[secret]]"),
            "{:?}",
            manifest.secret_warnings()[0]
        );
    }

    #[test]
    fn a_secret_entry_declares_one_portable_name() {
        let manifest = parse(
            "[[secret]]\nname = \"STRIPE_KEY\"\n\n[[secret]]\nname = \"JWT_SIGNING_KEY\"\n\n\
             [cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.secret.STRIPE_KEY]\nstore_id = \"0c2a3f\"\nsecret_name = \"stripe-key\"\n",
        )
        .expect("manifest parses");

        let secrets = &manifest.data().secret;
        assert_eq!(secrets.len(), 2);
        assert_eq!(secrets[0].name, "STRIPE_KEY");
        assert_eq!(secrets[1].name, "JWT_SIGNING_KEY");

        let backing = &manifest
            .cloudflare(None)
            .expect("base")
            .expect("cloudflare")
            .secret[&secrets[0].name];
        assert_eq!(backing.store_id, "0c2a3f");
        assert_eq!(backing.secret_name, "stripe-key");
    }

    #[test]
    fn two_secrets_cannot_share_a_name() {
        let error = parse("[[secret]]\nname = \"API_KEY\"\n\n[[secret]]\nname = \"API_KEY\"\n")
            .expect_err("one secret, one name");
        assert!(
            error.to_string().contains("twice") && error.to_string().contains("API_KEY"),
            "{error}"
        );
    }

    #[test]
    fn backing_a_secret_the_manifest_does_not_declare_is_rejected() {
        let error = parse(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.secret.STRIPE_KEY]\nstore_id = \"s\"\nsecret_name = \"stripe-key\"\n",
        )
        .expect_err("nothing declares STRIPE_KEY");
        assert!(
            error.to_string().contains("[cloudflare.secret.STRIPE_KEY]"),
            "{error}"
        );
        assert!(error.to_string().contains("[[secret]]"), "{error}");
    }

    #[test]
    fn an_overlay_backing_an_undeclared_secret_is_rejected_too() {
        let error = parse(
            "[[secret]]\nname = \"API_KEY\"\n\n\
             [cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.env.staging.secret.OTHER_KEY]\nstore_id = \"s\"\nsecret_name = \"o\"\n",
        )
        .expect_err("the overlay backs a secret nothing declares");
        assert!(
            error
                .to_string()
                .contains("cloudflare.env.staging.secret.OTHER_KEY"),
            "{error}"
        );
    }

    #[test]
    fn a_secret_cannot_also_be_a_plaintext_variable() {
        for (source, table) in [
            (
                "[[secret]]\nname = \"API_KEY\"\n\n[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
                 [cloudflare.vars]\nAPI_KEY = \"not-a-secret\"\n",
                "cloudflare.vars",
            ),
            (
                "[[secret]]\nname = \"API_KEY\"\n\n[aws.env]\nAPI_KEY = \"not-a-secret\"\n",
                "aws.env",
            ),
            // Only the overlay collides: the base has no such var, and the parse still fails.
            (
                "[[secret]]\nname = \"API_KEY\"\n\n[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
                 [cloudflare.env.staging.vars]\nAPI_KEY = \"not-a-secret\"\n",
                "cloudflare.env.staging.vars",
            ),
        ] {
            let error = parse(source).expect_err("one name, one kind of variable");
            let rendered = error.to_string();
            assert!(rendered.contains(table), "{table}: {rendered}");
            assert!(rendered.contains("API_KEY"), "{table}: {rendered}");
        }
    }

    #[test]
    fn a_placeholder_cannot_name_a_variable() {
        // The CLI expands `${FOO}`; `#[skyzen::main]` does not. A variable name that means two
        // things to the two consumers is refused where it is written.
        let wiring = database_wiring("backend = \"postgres\"\nurl_env = \"${FOO}\"\n")
            .expect_err("a placeholder is not a variable name")
            .to_string();
        // A `backend`-tagged wiring is buffered by serde before the payload struct sees it, which
        // costs the key's span: the error locates the wiring's table and quotes the value.
        assert!(wiring.contains("native.database.main"), "{wiring}");
        assert!(wiring.contains("${FOO}"), "{wiring}");
        assert!(wiring.contains("placeholder"), "{wiring}");

        // Everywhere the span survives, the field itself is named.
        let secret = parse("[[secret]]\nname = \"${FOO}\"\n")
            .expect_err("a placeholder is not a secret name")
            .to_string();
        assert!(secret.contains("secret.name"), "{secret}");
    }

    #[test]
    fn a_variable_name_is_an_identifier_or_it_is_not_a_name() {
        for rejected in ["FOO-BAR", "${X}", "1abc", "", "FOO BAR", "a.b"] {
            let error = VarName::try_from(rejected.to_owned())
                .expect_err("not an environment variable name");
            assert!(error.to_string().contains(rejected), "{rejected}: {error}");
        }
        for accepted in ["FOO", "_private", "a1", "STRIPE_KEY"] {
            let name = VarName::try_from(accepted.to_owned()).expect("an identifier");
            assert_eq!(name, accepted);
            assert_eq!(name.as_str(), accepted);
            assert_eq!(name.to_string(), accepted);
        }
    }

    #[test]
    fn the_raw_escape_hatch_accepts_arbitrary_tables() {
        let manifest = parse(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.raw]\nplacement = { mode = \"smart\" }\n\n\
             [[cloudflare.raw.vectorize]]\nbinding = \"VEC\"\nindex_name = \"docs\"\n",
        )
        .expect("manifest parses");

        let raw = &manifest
            .cloudflare(None)
            .expect("base")
            .expect("cloudflare section")
            .raw;
        assert_eq!(raw["placement"]["mode"].as_str(), Some("smart"));
        assert_eq!(raw["vectorize"].as_array().expect("array").len(), 1);
    }
}
