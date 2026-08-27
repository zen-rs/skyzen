//! Creating the Cloudflare resources a manifest declares.
//!
//! Before this existed the onboarding path for anything stateful was: run wrangler by hand, copy
//! the id it printed, paste it into `Skyzen.toml`. `skyzen provision` runs the same wrangler
//! commands and writes the ids back with `toml_edit`, so the manifest keeps its formatting and its
//! comments.

use crate::{
    output,
    providers::cloudflare::ids::{is_provisioned, ResourceKind, Slot},
};
use anyhow::{Context, Result};
use regex::Regex;
use skyzen_manifest::CloudflareSection;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};
use toml_edit::{value, DocumentMut, Item};

/// The wrangler.toml array a resource kind's entries live in, and the key holding its id.
const fn manifest_location(kind: ResourceKind) -> (&'static str, &'static str) {
    match kind {
        ResourceKind::KvNamespace => ("kv_namespaces", "id"),
        ResourceKind::D1Database => ("d1_databases", "database_id"),
        ResourceKind::R2Bucket => ("r2_buckets", "bucket_name"),
        ResourceKind::Queue => ("queues", "queue"),
    }
}

/// One resource that has to be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// Which binding needs it.
    pub slot: Slot,
}

impl Task {
    /// A one-line description for the plan and for `--dry-run`.
    pub fn describe(&self) -> String {
        format!(
            "create {} `{}` for binding `{}`",
            self.slot.kind.label(),
            self.slot.resource_name,
            self.slot.binding
        )
    }
}

/// Everything the manifest declares that Cloudflare does not have yet.
///
/// Entries whose id is already filled in are skipped, which is what makes running `provision`
/// twice harmless. R2 buckets and queues are addressed by name rather than by id, so they are
/// always attempted and an "already exists" answer is treated as success.
pub fn plan(config: &CloudflareSection) -> Vec<Task> {
    let mut tasks = Vec::new();

    for entry in &config.kv_namespaces {
        if !is_provisioned(entry.id.as_deref()) {
            tasks.push(Task {
                slot: Slot::kv(&entry.binding),
            });
        }
    }
    for entry in &config.d1_databases {
        if !is_provisioned(entry.database_id.as_deref()) {
            tasks.push(Task {
                slot: Slot::d1(&entry.binding, &entry.database_name),
            });
        }
    }
    for entry in &config.r2_buckets {
        tasks.push(Task {
            slot: Slot::r2(&entry.binding, &entry.bucket_name),
        });
    }
    for entry in &config.queues.producers {
        tasks.push(Task {
            slot: Slot::queue(Some(&entry.binding), &entry.queue),
        });
    }
    for entry in &config.queues.consumers {
        // A queue named by both a producer and a consumer is one queue: it is addressed by name,
        // and creating it twice would fail the second time.
        let slot = Slot::queue(None, &entry.queue);
        let already_planned = tasks.iter().any(|task| {
            task.slot.kind == slot.kind && task.slot.resource_name == slot.resource_name
        });
        if !already_planned {
            tasks.push(Task { slot });
        }
    }

    tasks
}

/// Run the plan, writing every returned id back into `manifest_path`.
///
/// # Errors
///
/// Fails when wrangler is absent or unauthenticated, when a create command fails for a reason
/// other than the resource already existing, when the id cannot be found in wrangler's output, or
/// when the manifest cannot be updated.
pub fn run(
    tasks: &[Task],
    manifest_path: &Path,
    environment: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    if tasks.is_empty() {
        output::step("every declared Cloudflare resource already has an id; nothing to provision");
        return Ok(());
    }

    if dry_run {
        for task in tasks {
            output::dry_run(task.describe());
        }
        return Ok(());
    }

    ensure_authenticated()?;

    let mut document = read_document(manifest_path)?;
    let mut updated = false;
    for task in tasks {
        output::step(task.describe());
        let Some(id) = create(&task.slot)? else {
            continue;
        };
        write_back(&mut document, environment, &task.slot, &id)?;
        updated = true;
        output::step(format!("  {} = \"{id}\"", id_key(task.slot.kind)));
    }

    if updated {
        fs::write(manifest_path, document.to_string())
            .with_context(|| format!("failed to write {}", manifest_path.display()))?;
        output::step(format!("updated {}", manifest_path.display()));
    }

    Ok(())
}

const fn id_key(kind: ResourceKind) -> &'static str {
    manifest_location(kind).1
}

/// Fail early when wrangler is missing or nobody is logged in.
///
/// Every create command would otherwise fail one at a time with the same authentication error,
/// after the first few have already been created.
fn ensure_authenticated() -> Result<()> {
    let output = Command::new("wrangler")
        .arg("whoami")
        .stdin(Stdio::null())
        .output()
        .context(
            "failed to launch `wrangler`; install it (`npm install -g wrangler`) before provisioning",
        )?;

    if output.status.success() {
        return Ok(());
    }

    anyhow::bail!(
        "`wrangler whoami` failed, so Skyzen cannot create resources on your account. \
         Run `wrangler login` (or set CLOUDFLARE_API_TOKEN) and try again.\n{}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// Create one resource, returning the id when the resource kind has one.
fn create(slot: &Slot) -> Result<Option<String>> {
    let output = Command::new("wrangler")
        .args(slot.kind.create_subcommand())
        .arg(&slot.resource_name)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to launch wrangler to create {}", slot.resource_name))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        if already_exists(&stdout) || already_exists(&stderr) {
            output::step(format!(
                "  {} `{}` already exists",
                slot.kind.label(),
                slot.resource_name
            ));
            return Ok(None);
        }
        anyhow::bail!(
            "wrangler failed to create {} `{}`:\n{}",
            slot.kind.label(),
            slot.resource_name,
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        );
    }

    if !slot.kind.yields_id() {
        return Ok(None);
    }

    let key = id_key(slot.kind);
    extract_id(&stdout, key)
        .or_else(|| extract_id(&stderr, key))
        .map(Some)
        .with_context(|| {
            format!(
                "wrangler created {} `{}` but its output contained no `{key}`; paste the id into \
                 Skyzen.toml by hand:\n{}",
                slot.kind.label(),
                slot.resource_name,
                stdout.trim()
            )
        })
}

fn already_exists(text: &str) -> bool {
    text.to_ascii_lowercase().contains("already exists")
}

/// Pull an id out of wrangler's output, which is TOML-ish in some versions and JSON in others.
fn extract_id(text: &str, key: &str) -> Option<String> {
    let pattern = format!(r#"['"]?{key}['"]?\s*[:=]\s*['"]([^'"]+)['"]"#);
    let regex = Regex::new(&pattern).ok()?;
    regex
        .captures(text)
        .and_then(|captures| captures.get(1))
        .map(|matched| matched.as_str().to_owned())
}

fn read_document(manifest_path: &Path) -> Result<DocumentMut> {
    let content = fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    content
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))
}

/// Set the id on the manifest entry the slot came from, preserving the file's formatting.
fn write_back(
    document: &mut DocumentMut,
    environment: Option<&str>,
    slot: &Slot,
    id: &str,
) -> Result<()> {
    let (array_key, id_key) = manifest_location(slot.kind);
    let section = section_path(environment);
    let mut item: &mut Item = document.as_item_mut();
    for key in &section {
        item = item
            .get_mut(key)
            .with_context(|| format!("Skyzen.toml has no [{}] section", section.join(".")))?;
    }

    let entries = item
        .get_mut(array_key)
        .and_then(Item::as_array_of_tables_mut)
        .with_context(|| {
            format!(
                "Skyzen.toml has no [[{}.{array_key}]] entries to update",
                section.join(".")
            )
        })?;

    let entry = entries
        .iter_mut()
        .find(|table| {
            table
                .get("binding")
                .and_then(Item::as_str)
                .is_some_and(|binding| binding == slot.binding)
        })
        .with_context(|| {
            format!(
                "Skyzen.toml has no [[{}.{array_key}]] entry bound as `{}`",
                section.join("."),
                slot.binding
            )
        })?;

    entry[id_key] = value(id);
    Ok(())
}

/// The manifest path the bindings for `environment` live under.
fn section_path(environment: Option<&str>) -> Vec<String> {
    let mut path = vec!["cloudflare".to_owned()];
    if let Some(environment) = environment {
        path.push("env".to_owned());
        path.push(environment.to_owned());
    }
    path
}

#[cfg(test)]
mod tests {
    use super::{extract_id, plan, section_path, write_back};
    use crate::providers::cloudflare::ids::{ResourceKind, Slot};
    use skyzen_manifest::Manifest;
    use toml_edit::DocumentMut;

    fn cloudflare(source: &str) -> skyzen_manifest::CloudflareSection {
        Manifest::parse(source, "Skyzen.toml", ".")
            .expect("valid manifest")
            .cloudflare(None)
            .expect("base")
            .expect("cloudflare section")
            .clone()
    }

    #[test]
    fn only_bindings_without_an_id_are_planned() {
        let config = cloudflare(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"SESSIONS\"\nid = \"already-there\"\n\n\
             [[cloudflare.d1_databases]]\nbinding = \"DB\"\ndatabase_name = \"app\"\n",
        );

        let planned: Vec<_> = plan(&config)
            .into_iter()
            .map(|task| (task.slot.kind, task.slot.binding))
            .collect();
        assert_eq!(
            planned,
            [
                (ResourceKind::KvNamespace, "CACHE".to_owned()),
                (ResourceKind::D1Database, "DB".to_owned()),
            ]
        );
    }

    #[test]
    fn a_queue_named_by_both_a_producer_and_a_consumer_is_created_once() {
        let config = cloudflare(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [[cloudflare.queues.producers]]\nbinding = \"JOBS\"\nqueue = \"jobs\"\n\n\
             [[cloudflare.queues.consumers]]\nqueue = \"jobs\"\n",
        );
        let queues = plan(&config)
            .into_iter()
            .filter(|task| task.slot.kind == ResourceKind::Queue)
            .count();
        assert_eq!(queues, 1);
    }

    #[test]
    fn ids_are_read_from_both_the_toml_and_the_json_shapes_wrangler_prints() {
        let toml_shape = "✨ Success!\n[[kv_namespaces]]\nbinding = \"CACHE\"\nid = \"abc123\"\n";
        assert_eq!(extract_id(toml_shape, "id").as_deref(), Some("abc123"));

        let json_shape = r#"{"id": "def456", "title": "worker-CACHE"}"#;
        assert_eq!(extract_id(json_shape, "id").as_deref(), Some("def456"));

        let d1 = "database_name = \"app\"\ndatabase_id = \"11111111-2222-3333-4444-555555555555\"";
        assert_eq!(
            extract_id(d1, "database_id").as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );

        assert_eq!(extract_id("nothing useful here", "id"), None);
    }

    #[test]
    fn the_write_back_preserves_comments_and_formatting() {
        let source = "# keep me\n[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [[cloudflare.kv_namespaces]]\n# and me\nbinding = \"CACHE\"\n";
        let mut document = source.parse::<DocumentMut>().expect("valid TOML");

        write_back(&mut document, None, &Slot::kv("CACHE"), "abc123").expect("write back");

        let updated = document.to_string();
        assert!(updated.contains("# keep me"), "{updated}");
        assert!(updated.contains("# and me"), "{updated}");
        assert!(updated.contains("id = \"abc123\""), "{updated}");
    }

    #[test]
    fn the_write_back_targets_the_selected_environments_bindings() {
        let source = "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"base\"\n\n\
             [[cloudflare.env.staging.kv_namespaces]]\nbinding = \"CACHE\"\n";
        let mut document = source.parse::<DocumentMut>().expect("valid TOML");

        write_back(
            &mut document,
            Some("staging"),
            &Slot::kv("CACHE"),
            "staging-id",
        )
        .expect("write back");

        let updated = document.to_string();
        assert!(updated.contains("id = \"base\""), "{updated}");
        assert!(updated.contains("id = \"staging-id\""), "{updated}");
    }

    #[test]
    fn an_unknown_binding_is_reported_rather_than_appended() {
        let mut document = "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n"
            .parse::<DocumentMut>()
            .expect("valid TOML");
        let error = write_back(&mut document, None, &Slot::kv("CACHE"), "abc")
            .expect_err("there is nothing to update");
        assert!(error.to_string().contains("kv_namespaces"), "{error}");
    }

    #[test]
    fn environment_bindings_live_under_the_env_table() {
        assert_eq!(section_path(None), ["cloudflare"]);
        assert_eq!(
            section_path(Some("staging")),
            ["cloudflare", "env", "staging"]
        );
    }
}
