use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

use anyhow::{Context, Result};
use toml_edit::{value, Array, DocumentMut, InlineTable, Item, Table, Value};

use crate::{
    args::Provider,
    manifest::{LoadedManifest, NativeDatabaseSection, NativeServiceSection},
};

#[derive(Debug, Default)]
struct DesiredDependency {
    features: BTreeSet<String>,
    disable_default_features: bool,
}

pub fn ensure_capability_deps(manifest: &LoadedManifest, provider: Option<Provider>) -> Result<()> {
    let cargo_toml_path = manifest.root_dir.join("Cargo.toml");
    if !cargo_toml_path.exists() {
        return Ok(());
    }

    let mut wanted = BTreeMap::<String, DesiredDependency>::new();
    let has_portable_capabilities =
        !manifest.data.service.is_empty() || !manifest.data.database.is_empty();
    if has_portable_capabilities {
        wanted.entry("skyzen-services".to_owned()).or_default();
    }

    if let Some(native) = &manifest.data.native {
        for service in native.service.values() {
            record_native_service_dependency(&mut wanted, service);
        }
        for database in native.database.values() {
            record_native_database_dependency(&mut wanted, database);
        }
    }

    let needs_cloudflare = manifest
        .data
        .cloudflare
        .as_ref()
        .is_some_and(|config| !config.service.is_empty() || !config.database.is_empty())
        || matches!(provider, Some(Provider::Cloudflare));
    if needs_cloudflare && has_portable_capabilities {
        wanted.entry("skyzen-cloudflare".to_owned()).or_default();
    }

    if wanted.is_empty() {
        return Ok(());
    }

    let content = fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("failed to read {}", cargo_toml_path.display()))?;
    let mut document = content
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", cargo_toml_path.display()))?;

    let mut changed = false;
    for (crate_name, desired) in &wanted {
        changed |= ensure_dependency(
            &mut document,
            "dependencies",
            crate_name,
            env!("CARGO_PKG_VERSION"),
            desired,
        )?;
    }

    if changed {
        fs::write(&cargo_toml_path, document.to_string())
            .with_context(|| format!("failed to write {}", cargo_toml_path.display()))?;
    }

    Ok(())
}

fn record_native_service_dependency(
    wanted: &mut BTreeMap<String, DesiredDependency>,
    service: &NativeServiceSection,
) {
    match service.backend.as_str() {
        "redis" => {
            wanted.entry("skyzen-redis".to_owned()).or_default();
        }
        "s3" => {
            wanted.entry("skyzen-s3".to_owned()).or_default();
        }
        "sqs" => {
            let desired = wanted.entry("skyzen-aws".to_owned()).or_default();
            desired.disable_default_features = true;
            desired.features.insert("sqs".to_owned());
        }
        "memory" => {
            wanted.entry("skyzen-test".to_owned()).or_default();
        }
        _ => {}
    }
}

fn record_native_database_dependency(
    wanted: &mut BTreeMap<String, DesiredDependency>,
    database: &NativeDatabaseSection,
) {
    if database.backend == "postgres" {
        let desired = wanted.entry("skyzen-services".to_owned()).or_default();
        desired.features.insert("postgres".to_owned());
        desired.features.insert("runtime-tokio-rustls".to_owned());
    }
}

fn ensure_dependency(
    document: &mut DocumentMut,
    section: &str,
    crate_name: &str,
    version: &str,
    desired: &DesiredDependency,
) -> Result<bool> {
    let table = ensure_table(document, section)?;
    if !table.contains_key(crate_name) {
        let mut dependency = InlineTable::new();
        dependency.insert("version", Value::from(version));
        if desired.disable_default_features {
            dependency.insert("default-features", Value::from(false));
        }
        if !desired.features.is_empty() {
            dependency.insert("features", Value::Array(features_array(&desired.features)));
        }
        table.insert(crate_name, Item::Value(Value::InlineTable(dependency)));
        return Ok(true);
    }

    let item = table
        .get_mut(crate_name)
        .with_context(|| format!("missing dependency item for `{crate_name}`"))?;
    match item {
        Item::Value(Value::String(existing_version)) => {
            let mut dependency = InlineTable::new();
            dependency.insert("version", Value::from(existing_version.value()));
            if desired.disable_default_features {
                dependency.insert("default-features", Value::from(false));
            }
            if !desired.features.is_empty() {
                dependency.insert("features", Value::Array(features_array(&desired.features)));
            }
            *item = Item::Value(Value::InlineTable(dependency));
            Ok(true)
        }
        Item::Value(Value::InlineTable(table)) => merge_inline_table(table, desired),
        Item::Table(table) => merge_table(table, desired),
        other => anyhow::bail!("unsupported dependency format for `{crate_name}`: {other:?}"),
    }
}

fn ensure_table<'a>(document: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table> {
    if !document.contains_key(name) {
        document.insert(name, Item::Table(Table::new()));
    }

    document[name]
        .as_table_mut()
        .with_context(|| format!("`{name}` is not a TOML table"))
}

fn merge_inline_table(table: &mut InlineTable, desired: &DesiredDependency) -> Result<bool> {
    let mut changed = if desired.disable_default_features
        && table.get("default-features").and_then(Value::as_bool) != Some(false)
    {
        table.insert("default-features", Value::from(false));
        true
    } else {
        false
    };

    if !desired.features.is_empty() {
        let mut merged = existing_features_inline(table.get("features"))?;
        let original_len = merged.len();
        merged.extend(desired.features.iter().cloned());
        if merged.len() != original_len || table.get("features").is_none() {
            table.insert("features", Value::Array(features_array(&merged)));
            changed = true;
        }
    }

    Ok(changed)
}

fn merge_table(table: &mut Table, desired: &DesiredDependency) -> Result<bool> {
    let mut changed = if desired.disable_default_features
        && table
            .get("default-features")
            .and_then(Item::as_value)
            .and_then(Value::as_bool)
            != Some(false)
    {
        table["default-features"] = value(false);
        true
    } else {
        false
    };

    if !desired.features.is_empty() {
        let mut merged = existing_features_table(table.get("features"))?;
        let original_len = merged.len();
        merged.extend(desired.features.iter().cloned());
        if merged.len() != original_len || table.get("features").is_none() {
            table["features"] = Item::Value(Value::Array(features_array(&merged)));
            changed = true;
        }
    }

    Ok(changed)
}

fn existing_features_inline(value: Option<&Value>) -> Result<BTreeSet<String>> {
    match value {
        None => Ok(BTreeSet::new()),
        Some(Value::Array(array)) => array
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .with_context(|| "dependency `features` array must contain strings")
            })
            .collect(),
        Some(other) => anyhow::bail!("dependency `features` must be an array, got {other:?}"),
    }
}

fn existing_features_table(item: Option<&Item>) -> Result<BTreeSet<String>> {
    match item {
        None => Ok(BTreeSet::new()),
        Some(Item::Value(Value::Array(array))) => array
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .with_context(|| "dependency `features` array must contain strings")
            })
            .collect(),
        Some(other) => anyhow::bail!("dependency `features` must be an array, got {other:?}"),
    }
}

fn features_array(features: &BTreeSet<String>) -> Array {
    let mut array = Array::new();
    for feature in features {
        array.push(feature.as_str());
    }
    array
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        CloudflareDatabaseSection, CloudflareSection, CloudflareServiceSection, DatabaseEntry,
        NativeDatabaseSection, NativeSection, NativeServiceSection, ServiceEntry, SkyzenManifest,
    };
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_project_dir() -> PathBuf {
        let mut path = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        path.push(format!("skyzen-cli-deps-{unique}"));
        fs::create_dir_all(&path).expect("failed to create temp project dir");
        path
    }

    #[test]
    fn adds_required_dependencies_for_portable_services_and_database() {
        let root_dir = temp_project_dir();
        let cargo_toml = root_dir.join("Cargo.toml");
        fs::write(
            &cargo_toml,
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");

        let manifest = LoadedManifest {
            root_dir,
            data: SkyzenManifest {
                service: vec![ServiceEntry {
                    name: "cache".to_owned(),
                    service_type: "kv".to_owned(),
                }],
                database: vec![DatabaseEntry {
                    name: "main".to_owned(),
                    database_type: "sql".to_owned(),
                }],
                native: Some(NativeSection {
                    service: BTreeMap::from([(
                        "cache".to_owned(),
                        NativeServiceSection {
                            backend: "redis".to_owned(),
                            url_env: Some("CACHE_URL".to_owned()),
                            bucket_env: None,
                        },
                    )]),
                    database: BTreeMap::from([(
                        "main".to_owned(),
                        NativeDatabaseSection {
                            backend: "postgres".to_owned(),
                            url_env: "DATABASE_URL".to_owned(),
                        },
                    )]),
                }),
                cloudflare: Some(CloudflareSection {
                    service: BTreeMap::from([(
                        "cache".to_owned(),
                        CloudflareServiceSection {
                            binding: "CACHE".to_owned(),
                        },
                    )]),
                    database: BTreeMap::from([(
                        "main".to_owned(),
                        CloudflareDatabaseSection {
                            binding: "DB".to_owned(),
                        },
                    )]),
                    ..CloudflareSection::default()
                }),
                ..SkyzenManifest::default()
            },
        };

        ensure_capability_deps(&manifest, Some(Provider::Cloudflare)).expect("deps sync");
        let updated = fs::read_to_string(&cargo_toml).expect("read Cargo.toml");
        assert!(updated.contains("skyzen-services"));
        assert!(updated.contains("skyzen-redis"));
        assert!(updated.contains("skyzen-cloudflare"));
        assert!(updated.contains("runtime-tokio-rustls"));
        assert!(updated.contains("postgres"));
    }
}
