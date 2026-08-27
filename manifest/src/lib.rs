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

mod merge;
pub mod migrations;
mod schema;

pub use merge::deep_merge;
pub use migrations::{MigrationFile, MigrationsError, DEFAULT_MIGRATIONS_DIR};
pub use schema::{
    AwsSection, AzureHttpMode, AzureQueueTrigger, AzureSection, CfAssets, CfAssetsNotFoundHandling,
    CfD1Database, CfDurableBinding, CfDurableMigration, CfDurableObjects, CfDurableRenamedClass,
    CfHandlers, CfKvNamespace, CfQueueConsumer, CfQueueProducer, CfQueues, CfR2Bucket,
    CfSecretsStoreSecret, CfServiceBinding, CfTriggers, CloudflareDatabaseSection,
    CloudflareSection, CloudflareServiceSection, DatabaseEntry, DatabaseType, LambdaArchitecture,
    NativeDatabaseBackend, NativeDatabaseSection, NativeQueueConsumer, NativeSection,
    NativeServiceBackend, NativeServiceSection, QueueTriggerError, ServiceEntry, ServiceType,
    SkyzenManifest, HTTP_FUNCTION_NAME,
};

use std::{
    collections::BTreeMap,
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
}

impl Manifest {
    /// Read and parse the manifest at `path`.
    ///
    /// `root_dir` — the project root every relative path in the manifest is resolved against — is
    /// taken to be the manifest's parent directory.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when the file cannot be read, is not valid TOML, does not match
    /// the schema, or has a malformed `[cloudflare.env]` table.
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
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
        Self::parse(&content, &path, root_dir)
    }

    /// Parse manifest `content` that came from `path`, rooted at `root_dir`.
    ///
    /// `path` is used only for error messages, so callers holding the content in memory (tests,
    /// or a manifest embedded in a template) can name whatever the user would recognize.
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
        let path = path.as_ref().to_path_buf();
        let mut document: toml::Table =
            toml::from_str(content).map_err(|source| ManifestError::Syntax {
                path: path.clone(),
                source: Box::new(source),
            })?;

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

        Ok(Self {
            path,
            root_dir: root_dir.into(),
            data,
            environments,
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
    use super::{Manifest, ManifestError, NativeServiceBackend, ServiceType};
    use std::time::Duration;

    fn parse(content: &str) -> Result<Manifest, ManifestError> {
        Manifest::parse(content, "Skyzen.toml", ".")
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
            data.native.as_ref().expect("native").service["cache"].backend,
            NativeServiceBackend::Memory
        );
        assert!(data.database[0].default);
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
