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
mod schema;

pub use merge::deep_merge;
pub use schema::{
    CfAssets, CfAssetsNotFoundHandling, CfD1Database, CfDurableBinding, CfDurableMigration,
    CfDurableObjects, CfDurableRenamedClass, CfHandlers, CfKvNamespace, CfQueueConsumer,
    CfQueueProducer, CfQueues, CfR2Bucket, CfSecretsStoreSecret, CfServiceBinding, CfTriggers,
    CloudflareDatabaseSection, CloudflareSection, CloudflareServiceSection, DatabaseEntry,
    DatabaseType, NativeDatabaseBackend, NativeDatabaseSection, NativeSection,
    NativeServiceBackend, NativeServiceSection, ServiceEntry, ServiceType, SkyzenManifest,
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
        let root_dir = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
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
        let manifest = parse("[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.env.staging]\nname = \"app-staging\"\n")
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
