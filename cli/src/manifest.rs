use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SkyzenManifest {
    #[serde(default)]
    pub service: Vec<ServiceEntry>,
    #[serde(default)]
    pub database: Vec<DatabaseEntry>,
    #[serde(default)]
    pub native: Option<NativeSection>,
    #[serde(default)]
    pub cloudflare: Option<CloudflareSection>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub service_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub database_type: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NativeSection {
    #[serde(default)]
    pub service: BTreeMap<String, NativeServiceSection>,
    #[serde(default)]
    pub database: BTreeMap<String, NativeDatabaseSection>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct NativeServiceSection {
    pub backend: String,
    pub url_env: Option<String>,
    pub bucket_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct NativeDatabaseSection {
    pub backend: String,
    pub url_env: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CloudflareSection {
    pub name: Option<String>,
    pub main: Option<String>,
    pub compatibility_date: Option<String>,
    #[serde(default)]
    pub compatibility_flags: Vec<String>,
    pub account_id: Option<String>,
    pub workers_dev: Option<bool>,
    pub route: Option<String>,
    pub zone_id: Option<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub triggers: CfTriggers,
    #[serde(default)]
    pub service: BTreeMap<String, CloudflareServiceSection>,
    #[serde(default)]
    pub database: BTreeMap<String, CloudflareDatabaseSection>,
    #[serde(default)]
    pub kv_namespaces: Vec<CfKvNamespace>,
    #[serde(default)]
    pub r2_buckets: Vec<CfR2Bucket>,
    #[serde(default)]
    pub d1_databases: Vec<CfD1Database>,
    #[serde(default)]
    pub queues: CfQueues,
    #[serde(default)]
    pub durable_objects: CfDurableObjects,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfTriggers {
    #[serde(default)]
    pub crons: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CloudflareServiceSection {
    pub binding: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CloudflareDatabaseSection {
    pub binding: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfKvNamespace {
    pub binding: String,
    pub id: String,
    pub preview_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfR2Bucket {
    pub binding: String,
    pub bucket_name: String,
    pub preview_bucket_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfD1Database {
    pub binding: String,
    pub database_name: String,
    pub database_id: String,
    pub preview_database_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfQueues {
    #[serde(default)]
    pub producers: Vec<CfQueueProducer>,
    #[serde(default)]
    pub consumers: Vec<CfQueueConsumer>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfQueueProducer {
    pub binding: String,
    pub queue: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfQueueConsumer {
    pub queue: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfDurableObjects {
    #[serde(default)]
    pub bindings: Vec<CfDurableBinding>,
    #[serde(default)]
    pub migrations: Vec<CfDurableMigration>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfDurableBinding {
    pub name: String,
    pub class_name: String,
    pub script_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfDurableMigration {
    pub tag: String,
    #[serde(default)]
    pub new_classes: Vec<String>,
    #[serde(default)]
    pub new_sqlite_classes: Vec<String>,
    #[serde(default)]
    pub deleted_classes: Vec<String>,
    #[serde(default)]
    pub renamed_classes: Vec<CfDurableRenamedClass>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CfDurableRenamedClass {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone)]
pub struct LoadedManifest {
    pub root_dir: PathBuf,
    pub data: SkyzenManifest,
}

impl LoadedManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let full_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .context("failed to read current directory")?
                .join(path)
        };
        let root_dir = full_path
            .parent()
            .context("manifest path has no parent directory")?
            .to_path_buf();
        let content = fs::read_to_string(&full_path)
            .with_context(|| format!("failed to read {}", full_path.display()))?;
        let data: SkyzenManifest = toml::from_str(&content)
            .with_context(|| format!("failed to parse {}", full_path.display()))?;
        Ok(Self { root_dir, data })
    }
}
