//! Rendering `wrangler.toml` from `Skyzen.toml`.
//!
//! The renderer is a `Serialize` mirror of wrangler's schema emitted with `toml::to_string_pretty`,
//! not a `push_str` assembly with a hand-written escaper. The escaper only handled backslash and
//! double quote, so a `[cloudflare.vars]` value containing a newline or a tab emitted a
//! `wrangler.toml` that failed to parse; going through serde removes that class of bug entirely
//! and turns a renamed field into a compile error.

use crate::providers::cloudflare::ids;
use anyhow::{Context, Result};
use serde::Serialize;
use skyzen_manifest::{deep_merge, CloudflareSection};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// What to do about a binding whose resource id the manifest does not carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdPolicy {
    /// Substitute a deterministic local id.
    ///
    /// `wrangler dev --local` never contacts Cloudflare — it keys miniflare's own storage by the
    /// binding — so requiring a provisioned id here would make `skyzen new && skyzen dev` fail on
    /// the very first run, and would make provisioning (which needs `wrangler login`) a
    /// prerequisite for offline development.
    LocalPlaceholder,
    /// Fail, naming `skyzen provision`. A deploy binds to real infrastructure.
    RequireProvisioned,
}

/// Everything the renderer needs about one build.
#[derive(Debug)]
pub struct RenderRequest<'a> {
    /// The base `[cloudflare]` section.
    pub base: &'a CloudflareSection,
    /// Each `[cloudflare.env.<name>]` overlay, already resolved against the base.
    pub environments: Vec<(String, &'a CloudflareSection)>,
    /// The project root, which `main` is made relative to.
    pub root_dir: &'a Path,
    /// The generated worker entry point.
    pub entry_js_path: &'a Path,
    /// The directory the rendered `wrangler.toml` will live in.
    pub wrangler_dir: PathBuf,
    /// How to treat a binding with no resource id.
    pub id_policy: IdPolicy,
}

/// Render the complete `wrangler.toml`, including every declared environment.
///
/// # Errors
///
/// Fails when a required key is missing, when a binding needs a resource id that
/// [`IdPolicy::RequireProvisioned`] will not substitute, or when the result cannot be serialized.
pub fn render(request: &RenderRequest<'_>) -> Result<String> {
    let base_name = worker_name(request.base, request.root_dir);
    let main = relative_posix_path(&request.wrangler_dir, request.entry_js_path)?;

    let mut document = to_table(&section_config(
        request.base,
        &base_name,
        &main,
        request.id_policy,
    )?)?;
    deep_merge(&mut document, request.base.raw.clone());

    if !request.environments.is_empty() {
        let mut environments = toml::Table::new();
        for (name, section) in &request.environments {
            // Wrangler's named environments do NOT inherit bindings — `kv_namespaces` and friends
            // are non-inheritable keys — so the environment's section has to carry the *complete*
            // merged configuration rather than only the overlay's diff.
            let environment_name = environment_worker_name(section, &base_name, name);
            let mut table = to_table(&section_config(
                section,
                &environment_name,
                &main,
                request.id_policy,
            )?)?;
            deep_merge(&mut table, section.raw.clone());
            environments.insert(name.clone(), toml::Value::Table(table));
        }
        document.insert("env".to_owned(), toml::Value::Table(environments));
    }

    toml::to_string_pretty(&document).context("failed to serialize the generated wrangler.toml")
}

/// The Worker name for a named environment.
///
/// Wrangler suffixes the Worker name with the environment when the environment does not set its
/// own `name`. Skyzen emits the resulting name explicitly instead of relying on that, so the
/// generated file says which Worker each environment deploys to.
fn environment_worker_name(
    section: &CloudflareSection,
    base_name: &str,
    environment: &str,
) -> String {
    match section.name.as_deref() {
        Some(name) if name != base_name => name.to_owned(),
        _ => format!("{base_name}-{environment}"),
    }
}

fn worker_name(config: &CloudflareSection, root_dir: &Path) -> String {
    config.name.clone().unwrap_or_else(|| {
        root_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("skyzen-app")
            .to_owned()
    })
}

fn to_table<T: Serialize>(value: &T) -> Result<toml::Table> {
    match toml::Value::try_from(value).context("failed to serialize the wrangler configuration")? {
        toml::Value::Table(table) => Ok(table),
        other => anyhow::bail!("the wrangler configuration serialized to {other}, not a table"),
    }
}

fn section_config(
    config: &CloudflareSection,
    name: &str,
    main: &str,
    id_policy: IdPolicy,
) -> Result<WranglerConfig> {
    let compatibility_date = config
        .compatibility_date
        .as_deref()
        .map(str::trim)
        .filter(|date| !date.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cloudflare.compatibility_date is required (example: compatibility_date = \"2025-02-01\")"
            )
        })?
        .to_owned();

    Ok(WranglerConfig {
        name: name.to_owned(),
        main: main.to_owned(),
        compatibility_date,
        compatibility_flags: config.compatibility_flags.clone(),
        account_id: config.account_id.clone(),
        workers_dev: config.workers_dev,
        route: config.route.clone(),
        zone_id: config.zone_id.clone(),
        vars: config.vars.clone(),
        triggers: (!config.triggers.crons.is_empty()).then(|| WranglerTriggers {
            crons: config.triggers.crons.clone(),
        }),
        assets: config.assets.as_ref().map(assets_config),
        queues: queues_config(config),
        durable_objects: durable_objects_config(config),
        kv_namespaces: kv_config(config, id_policy)?,
        r2_buckets: r2_config(config),
        d1_databases: d1_config(config, id_policy)?,
        services: config
            .services
            .iter()
            .map(|entry| WranglerServiceBinding {
                binding: entry.binding.clone(),
                service: entry.service.clone(),
                environment: entry.environment.clone(),
            })
            .collect(),
        // The binding is the `[[secret]]` name: it is what the map is keyed by, what the
        // application reads the secret under, and what wrangler creates the `env.NAME` object as.
        secrets_store_secrets: config
            .secret
            .iter()
            .map(|(binding, entry)| WranglerSecretsStoreSecret {
                binding: binding.to_string(),
                store_id: entry.store_id.clone(),
                secret_name: entry.secret_name.clone(),
            })
            .collect(),
        migrations: migrations_config(config),
    })
}

fn assets_config(assets: &skyzen_manifest::CfAssets) -> WranglerAssets {
    WranglerAssets {
        directory: assets.directory.clone(),
        binding: assets.binding.clone(),
        not_found_handling: assets
            .not_found_handling
            .map(|handling| handling.as_str().to_owned()),
        run_worker_first: assets.run_worker_first,
    }
}

fn kv_config(config: &CloudflareSection, id_policy: IdPolicy) -> Result<Vec<WranglerKvNamespace>> {
    config
        .kv_namespaces
        .iter()
        .map(|entry| {
            Ok(WranglerKvNamespace {
                binding: entry.binding.clone(),
                id: ids::resolve(
                    entry.id.as_deref(),
                    &ids::Slot::kv(&entry.binding),
                    id_policy,
                )?,
                preview_id: entry.preview_id.clone(),
            })
        })
        .collect()
}

fn r2_config(config: &CloudflareSection) -> Vec<WranglerR2Bucket> {
    config
        .r2_buckets
        .iter()
        .map(|entry| WranglerR2Bucket {
            binding: entry.binding.clone(),
            bucket_name: entry.bucket_name.clone(),
            preview_bucket_name: entry.preview_bucket_name.clone(),
        })
        .collect()
}

fn d1_config(config: &CloudflareSection, id_policy: IdPolicy) -> Result<Vec<WranglerD1Database>> {
    config
        .d1_databases
        .iter()
        .map(|entry| {
            Ok(WranglerD1Database {
                binding: entry.binding.clone(),
                database_name: entry.database_name.clone(),
                database_id: ids::resolve(
                    entry.database_id.as_deref(),
                    &ids::Slot::d1(&entry.binding, &entry.database_name),
                    id_policy,
                )?,
                preview_database_id: entry.preview_database_id.clone(),
            })
        })
        .collect()
}

fn queues_config(config: &CloudflareSection) -> Option<WranglerQueues> {
    (!config.queues.producers.is_empty() || !config.queues.consumers.is_empty()).then(|| {
        WranglerQueues {
            producers: config
                .queues
                .producers
                .iter()
                .map(|entry| WranglerQueueProducer {
                    binding: entry.binding.clone(),
                    queue: entry.queue.clone(),
                    delivery_delay: entry.delivery_delay,
                })
                .collect(),
            consumers: config
                .queues
                .consumers
                .iter()
                .map(|entry| WranglerQueueConsumer {
                    queue: entry.queue.clone(),
                    max_batch_size: entry.max_batch_size,
                    max_batch_timeout: entry.max_batch_timeout,
                    max_retries: entry.max_retries,
                    dead_letter_queue: entry.dead_letter_queue.clone(),
                    max_concurrency: entry.max_concurrency,
                    retry_delay: entry.retry_delay,
                })
                .collect(),
        }
    })
}

fn durable_objects_config(config: &CloudflareSection) -> Option<WranglerDurableObjects> {
    (!config.durable_objects.bindings.is_empty()).then(|| WranglerDurableObjects {
        bindings: config
            .durable_objects
            .bindings
            .iter()
            .map(|entry| WranglerDurableBinding {
                name: entry.name.clone(),
                class_name: entry.class_name.clone(),
                script_name: entry.script_name.clone(),
            })
            .collect(),
    })
}

fn migrations_config(config: &CloudflareSection) -> Vec<WranglerMigration> {
    config
        .durable_objects
        .migrations
        .iter()
        .map(|entry| WranglerMigration {
            tag: entry.tag.clone(),
            new_classes: entry.new_classes.clone(),
            new_sqlite_classes: entry.new_sqlite_classes.clone(),
            deleted_classes: entry.deleted_classes.clone(),
            renamed_classes: entry
                .renamed_classes
                .iter()
                .map(|renamed| WranglerRenamedClass {
                    from: renamed.from.clone(),
                    to: renamed.to.clone(),
                })
                .collect(),
        })
        .collect()
}

fn relative_posix_path(from_dir: &Path, to_path: &Path) -> Result<String> {
    let relative = pathdiff::diff_paths(to_path, from_dir).with_context(|| {
        format!(
            "failed to derive relative path from {} to {}",
            from_dir.display(),
            to_path.display()
        )
    })?;
    let value = relative
        .to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", relative.display()))?;
    Ok(value.replace('\\', "/"))
}

/// Wrangler's top-level configuration.
///
/// Scalars are declared before tables and arrays of tables because that is the order TOML requires
/// them to be emitted in.
#[derive(Debug, Serialize)]
struct WranglerConfig {
    name: String,
    main: String,
    compatibility_date: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    compatibility_flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workers_dev: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    zone_id: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    vars: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    triggers: Option<WranglerTriggers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assets: Option<WranglerAssets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queues: Option<WranglerQueues>,
    #[serde(skip_serializing_if = "Option::is_none")]
    durable_objects: Option<WranglerDurableObjects>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    kv_namespaces: Vec<WranglerKvNamespace>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    r2_buckets: Vec<WranglerR2Bucket>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    d1_databases: Vec<WranglerD1Database>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    services: Vec<WranglerServiceBinding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    secrets_store_secrets: Vec<WranglerSecretsStoreSecret>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    migrations: Vec<WranglerMigration>,
}

#[derive(Debug, Serialize)]
struct WranglerTriggers {
    crons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct WranglerAssets {
    directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    binding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    not_found_handling: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_worker_first: Option<bool>,
}

#[derive(Debug, Serialize)]
struct WranglerKvNamespace {
    binding: String,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WranglerR2Bucket {
    binding: String,
    bucket_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_bucket_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct WranglerD1Database {
    binding: String,
    database_name: String,
    database_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_database_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WranglerQueues {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    producers: Vec<WranglerQueueProducer>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    consumers: Vec<WranglerQueueConsumer>,
}

#[derive(Debug, Serialize)]
struct WranglerQueueProducer {
    binding: String,
    queue: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery_delay: Option<u32>,
}

#[derive(Debug, Serialize)]
struct WranglerQueueConsumer {
    queue: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_batch_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_batch_timeout: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dead_letter_queue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_concurrency: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_delay: Option<u32>,
}

#[derive(Debug, Serialize)]
struct WranglerDurableObjects {
    bindings: Vec<WranglerDurableBinding>,
}

#[derive(Debug, Serialize)]
struct WranglerDurableBinding {
    name: String,
    class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    script_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct WranglerServiceBinding {
    binding: String,
    service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<String>,
}

#[derive(Debug, Serialize)]
struct WranglerSecretsStoreSecret {
    binding: String,
    store_id: String,
    secret_name: String,
}

#[derive(Debug, Serialize)]
struct WranglerMigration {
    tag: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    new_classes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    new_sqlite_classes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deleted_classes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    renamed_classes: Vec<WranglerRenamedClass>,
}

#[derive(Debug, Serialize)]
struct WranglerRenamedClass {
    from: String,
    to: String,
}

#[cfg(test)]
mod tests {
    use super::{render, IdPolicy, RenderRequest};
    use skyzen_manifest::{CloudflareSection, Manifest};
    use std::path::{Path, PathBuf};

    fn request<'a>(
        base: &'a CloudflareSection,
        environments: Vec<(String, &'a CloudflareSection)>,
        id_policy: IdPolicy,
    ) -> RenderRequest<'a> {
        RenderRequest {
            base,
            environments,
            root_dir: Path::new("/tmp/app"),
            entry_js_path: Path::new("/tmp/app/dist/worker.js"),
            wrangler_dir: PathBuf::from("/tmp/app/.skyzen/gen"),
            id_policy,
        }
    }

    fn manifest(source: &str) -> Manifest {
        Manifest::parse(source, "Skyzen.toml", "/tmp/app").expect("valid manifest")
    }

    fn render_base(source: &str, id_policy: IdPolicy) -> String {
        let manifest = manifest(source);
        let base = manifest
            .cloudflare(None)
            .expect("base")
            .expect("cloudflare section");
        render(&request(base, Vec::new(), id_policy)).expect("render")
    }

    #[test]
    fn renders_every_modelled_binding_kind() {
        let rendered = render_base(
            "[[secret]]\nname = \"API_KEY\"\n\n\
             [cloudflare]\nname = \"skyzen-worker\"\nmain = \"dist/worker.js\"\n\
             compatibility_date = \"2025-02-01\"\ncompatibility_flags = [\"nodejs_compat\"]\n\
             account_id = \"abc\"\nworkers_dev = true\n\n\
             [cloudflare.vars]\nAPP_ENV = \"dev\"\n\n\
             [cloudflare.triggers]\ncrons = [\"*/10 * * * *\"]\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"123\"\npreview_id = \"456\"\n\n\
             [[cloudflare.r2_buckets]]\nbinding = \"UPLOADS\"\nbucket_name = \"uploads\"\n\n\
             [[cloudflare.d1_databases]]\nbinding = \"DB\"\ndatabase_name = \"app\"\ndatabase_id = \"d1-id\"\n\n\
             [[cloudflare.queues.producers]]\nbinding = \"JOBS\"\nqueue = \"jobs\"\ndelivery_delay = 30\n\n\
             [[cloudflare.queues.consumers]]\nqueue = \"jobs\"\nmax_batch_size = 10\ndead_letter_queue = \"jobs-dlq\"\n\n\
             [[cloudflare.services]]\nbinding = \"AUTH\"\nservice = \"auth-worker\"\nenvironment = \"production\"\n\n\
             [cloudflare.assets]\ndirectory = \"public\"\nbinding = \"ASSETS\"\nnot_found_handling = \"single-page-application\"\n\n\
             [cloudflare.secret.API_KEY]\nstore_id = \"store\"\nsecret_name = \"api-key\"\n\n\
             [[cloudflare.durable_objects.bindings]]\nname = \"STATE\"\nclass_name = \"State\"\n\n\
             [[cloudflare.durable_objects.migrations]]\ntag = \"v1\"\nnew_sqlite_classes = [\"State\"]\n",
            IdPolicy::RequireProvisioned,
        );

        // The result must be TOML wrangler can read back, not merely a string that looks right.
        let parsed: toml::Table = toml::from_str(&rendered).expect("valid TOML");
        assert_eq!(parsed["name"].as_str(), Some("skyzen-worker"));
        assert_eq!(parsed["main"].as_str(), Some("../../dist/worker.js"));
        assert_eq!(parsed["vars"]["APP_ENV"].as_str(), Some("dev"));
        assert_eq!(parsed["kv_namespaces"][0]["id"].as_str(), Some("123"));
        assert_eq!(
            parsed["queues"]["producers"][0]["delivery_delay"].as_integer(),
            Some(30)
        );
        assert_eq!(
            parsed["queues"]["consumers"][0]["dead_letter_queue"].as_str(),
            Some("jobs-dlq")
        );
        assert_eq!(
            parsed["services"][0]["service"].as_str(),
            Some("auth-worker")
        );
        assert_eq!(
            parsed["assets"]["not_found_handling"].as_str(),
            Some("single-page-application")
        );
        assert_eq!(
            parsed["secrets_store_secrets"][0]["secret_name"].as_str(),
            Some("api-key")
        );
        assert_eq!(
            parsed["durable_objects"]["bindings"][0]["name"].as_str(),
            Some("STATE")
        );
        assert_eq!(parsed["migrations"][0]["tag"].as_str(), Some("v1"));
        assert_eq!(
            parsed["triggers"]["crons"][0].as_str(),
            Some("*/10 * * * *")
        );
    }

    #[test]
    fn a_control_character_in_a_var_survives_a_round_trip() {
        // The hand-written escaper this replaced only handled `\` and `"`, so a newline in a var
        // emitted a wrangler.toml that failed to parse.
        let rendered = render_base(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.vars]\nBANNER = \"line1\\nline2\\ttabbed\"\nWINDOWS = 'C:\\path'\n",
            IdPolicy::RequireProvisioned,
        );

        let parsed: toml::Table = toml::from_str(&rendered).expect("valid TOML");
        assert_eq!(
            parsed["vars"]["BANNER"].as_str(),
            Some("line1\nline2\ttabbed")
        );
        assert_eq!(parsed["vars"]["WINDOWS"].as_str(), Some("C:\\path"));
    }

    #[test]
    fn the_raw_escape_hatch_is_deep_merged_verbatim() {
        let rendered = render_base(
            "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.vars]\nKEPT = \"yes\"\n\n\
             [cloudflare.raw]\nworkers_dev = false\n\n\
             [cloudflare.raw.vars]\nADDED = \"from-raw\"\n\n\
             [cloudflare.raw.observability]\nenabled = true\n\n\
             [[cloudflare.raw.vectorize]]\nbinding = \"VEC\"\nindex_name = \"docs\"\n",
            IdPolicy::RequireProvisioned,
        );

        let parsed: toml::Table = toml::from_str(&rendered).expect("valid TOML");
        // A table merges key by key: the rendered var survives alongside the raw one.
        assert_eq!(parsed["vars"]["KEPT"].as_str(), Some("yes"));
        assert_eq!(parsed["vars"]["ADDED"].as_str(), Some("from-raw"));
        // A scalar replaces, so raw can override what Skyzen rendered.
        assert_eq!(parsed["workers_dev"].as_bool(), Some(false));
        // Keys Skyzen does not model reach wrangler untouched.
        assert_eq!(parsed["observability"]["enabled"].as_bool(), Some(true));
        assert_eq!(parsed["vectorize"][0]["binding"].as_str(), Some("VEC"));
    }

    #[test]
    fn an_environment_carries_the_complete_merged_configuration() {
        let manifest = manifest(
            "[cloudflare]\nname = \"app\"\ncompatibility_date = \"2025-02-01\"\nworkers_dev = true\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"base-id\"\n\n\
             [cloudflare.env.staging]\nworkers_dev = false\n\n\
             [[cloudflare.env.staging.kv_namespaces]]\nbinding = \"CACHE\"\nid = \"staging-id\"\n",
        );
        let base = manifest.cloudflare(None).expect("base").expect("section");
        let staging = manifest
            .cloudflare(Some("staging"))
            .expect("staging")
            .expect("section");

        let rendered = render(&request(
            base,
            vec![("staging".to_owned(), staging)],
            IdPolicy::RequireProvisioned,
        ))
        .expect("render");
        let parsed: toml::Table = toml::from_str(&rendered).expect("valid TOML");

        let environment = &parsed["env"]["staging"];
        // Bindings are non-inheritable in wrangler, so the environment must restate them.
        assert_eq!(
            environment["kv_namespaces"][0]["id"].as_str(),
            Some("staging-id")
        );
        assert_eq!(environment["workers_dev"].as_bool(), Some(false));
        // Inherited keys the overlay did not touch are restated too.
        assert_eq!(
            environment["compatibility_date"].as_str(),
            Some("2025-02-01")
        );
        // The environment's Worker name is spelled out rather than left to wrangler's suffixing.
        assert_eq!(environment["name"].as_str(), Some("app-staging"));
        assert_eq!(parsed["name"].as_str(), Some("app"));
    }

    #[test]
    fn an_environment_may_name_its_own_worker() {
        let manifest = manifest(
            "[cloudflare]\nname = \"app\"\ncompatibility_date = \"2025-02-01\"\n\n\
             [cloudflare.env.prod]\nname = \"app-production\"\n",
        );
        let base = manifest.cloudflare(None).expect("base").expect("section");
        let prod = manifest
            .cloudflare(Some("prod"))
            .expect("prod")
            .expect("section");

        let rendered = render(&request(
            base,
            vec![("prod".to_owned(), prod)],
            IdPolicy::RequireProvisioned,
        ))
        .expect("render");
        let parsed: toml::Table = toml::from_str(&rendered).expect("valid TOML");
        assert_eq!(
            parsed["env"]["prod"]["name"].as_str(),
            Some("app-production")
        );
    }

    #[test]
    fn a_deploy_refuses_an_unprovisioned_binding_but_local_dev_substitutes_one() {
        let source = "[cloudflare]\ncompatibility_date = \"2025-02-01\"\n\n\
             [[cloudflare.kv_namespaces]]\nbinding = \"CACHE\"\n";

        let manifest = manifest(source);
        let base = manifest.cloudflare(None).expect("base").expect("section");
        let error = render(&request(base, Vec::new(), IdPolicy::RequireProvisioned))
            .expect_err("a deploy binds to real infrastructure");
        assert!(error.to_string().contains("skyzen provision"), "{error}");

        let rendered = render_base(source, IdPolicy::LocalPlaceholder);
        let parsed: toml::Table = toml::from_str(&rendered).expect("valid TOML");
        assert!(parsed["kv_namespaces"][0]["id"]
            .as_str()
            .expect("an id")
            .contains("local"));
    }

    #[test]
    fn a_missing_compatibility_date_names_the_key_to_add() {
        let manifest = manifest("[cloudflare]\nname = \"app\"\n");
        let base = manifest.cloudflare(None).expect("base").expect("section");
        let error = render(&request(base, Vec::new(), IdPolicy::LocalPlaceholder))
            .expect_err("compatibility_date is required");
        assert!(error.to_string().contains("compatibility_date"), "{error}");
    }
}
