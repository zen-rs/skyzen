//! The typed `Skyzen.toml` schema.
//!
//! Every struct carries `#[serde(deny_unknown_fields)]`: a mistyped key is a hard error rather
//! than a silently dropped binding. String-valued discriminants (`type`, `backend`) are modelled
//! as enums so an unsupported value is rejected by the parser instead of by a `match` arm buried
//! in a consumer.

use serde::Deserialize;
use std::collections::BTreeMap;

/// A parsed `Skyzen.toml` document.
///
/// The whole file is optional: an application can wire every service by hand in Rust. When the
/// file exists, `#[skyzen::main]` reads it to generate wiring and the `skyzen` CLI reads it to
/// render provider configuration, so both consumers see exactly this schema.
// `Eq` stops at `CloudflareSection`, whose `raw` escape hatch is a `toml::Table` (only
// `PartialEq`, because TOML floats are).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkyzenManifest {
    /// `[[service]]` — logical portable services (KV, object storage, queues).
    #[serde(default)]
    pub service: Vec<ServiceEntry>,
    /// `[[database]]` — logical portable SQL databases.
    #[serde(default)]
    pub database: Vec<DatabaseEntry>,
    /// `[native]` — how the portable capabilities are backed on native targets.
    #[serde(default)]
    pub native: Option<NativeSection>,
    /// `[cloudflare]` — Cloudflare Workers wiring and deployment configuration.
    #[serde(default)]
    pub cloudflare: Option<CloudflareSection>,
}

/// One `[[service]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceEntry {
    /// Logical name, used to key the wiring sections and to name the generated extractor type.
    pub name: String,
    /// Which portable service trait this entry provides.
    #[serde(rename = "type")]
    pub service_type: ServiceType,
}

/// The portable service kinds a `[[service]]` entry can declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    /// A key/value store (`skyzen_services::Kv`).
    Kv,
    /// An object store (`skyzen_services::Storage`).
    Storage,
    /// A message queue (`skyzen_services::Queue`).
    Queue,
}

impl ServiceType {
    /// The manifest spelling of this service type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Storage => "storage",
            Self::Queue => "queue",
        }
    }
}

/// One `[[database]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseEntry {
    /// Logical name, used to key the wiring sections and to name the generated extractor type.
    pub name: String,
    /// Which portable database trait this entry provides.
    #[serde(rename = "type")]
    pub database_type: DatabaseType,
    /// Whether a bare `Db` extractor resolves to this entry. At most one entry may set it.
    #[serde(default)]
    pub default: bool,
}

/// The portable database kinds a `[[database]]` entry can declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseType {
    /// A SQL database (`skyzen_services::Db`).
    Sql,
}

impl DatabaseType {
    /// The manifest spelling of this database type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sql => "sql",
        }
    }
}

/// `[native]` — native-target wiring for the portable capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSection {
    /// `[native.service.<name>]`, keyed by the `[[service]]` name.
    #[serde(default)]
    pub service: BTreeMap<String, NativeServiceSection>,
    /// `[native.database.<name>]`, keyed by the `[[database]]` name.
    #[serde(default)]
    pub database: BTreeMap<String, NativeDatabaseSection>,
}

/// `[native.service.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeServiceSection {
    /// Which backend crate provides the service natively.
    pub backend: NativeServiceBackend,
    /// Environment variable holding the connection URL (Redis, SQS).
    #[serde(default)]
    pub url_env: Option<String>,
    /// Environment variable holding the bucket name (S3).
    #[serde(default)]
    pub bucket_env: Option<String>,
}

/// The native backends a portable service can be wired to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeServiceBackend {
    /// `skyzen-redis` — a KV store backed by Redis.
    Redis,
    /// `skyzen-s3` — object storage backed by an S3-compatible endpoint.
    S3,
    /// `skyzen-aws` (`sqs` feature) — a queue backed by Amazon SQS.
    Sqs,
    /// `skyzen-test` — an in-process mock, for local development and tests.
    Memory,
}

impl NativeServiceBackend {
    /// The manifest spelling of this backend.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Redis => "redis",
            Self::S3 => "s3",
            Self::Sqs => "sqs",
            Self::Memory => "memory",
        }
    }
}

/// `[native.database.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDatabaseSection {
    /// Which SQL driver backs the database natively.
    pub backend: NativeDatabaseBackend,
    /// Environment variable holding the connection URL.
    pub url_env: String,
}

/// The native SQL drivers a portable database can be wired to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeDatabaseBackend {
    /// PostgreSQL, through `skyzen-services`' `postgres` feature.
    Postgres,
    /// MySQL, through `skyzen-services`' `mysql` feature.
    Mysql,
    /// SQLite, through `skyzen-services`' `sqlite` feature.
    Sqlite,
}

impl NativeDatabaseBackend {
    /// The manifest spelling of this backend.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::Sqlite => "sqlite",
        }
    }
}

/// `[cloudflare]` — Workers wiring and deployment configuration.
///
/// Note the two different `service` keys: `[cloudflare.service.<name>]` (a map) wires a
/// **portable** `[[service]]` entry to a Cloudflare binding, whereas `[[cloudflare.services]]`
/// (an array) declares a wrangler **service binding** to another Worker. They are unrelated.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareSection {
    /// Worker name. Defaults to the project directory name.
    #[serde(default)]
    pub name: Option<String>,
    /// Worker entry path relative to the project root. Defaults to `dist/worker.js`.
    #[serde(default)]
    pub main: Option<String>,
    /// Workers compatibility date. Required for a build.
    #[serde(default)]
    pub compatibility_date: Option<String>,
    /// Workers compatibility flags.
    #[serde(default)]
    pub compatibility_flags: Vec<String>,
    /// Cloudflare account id.
    #[serde(default)]
    pub account_id: Option<String>,
    /// Whether to publish on the `*.workers.dev` subdomain.
    #[serde(default)]
    pub workers_dev: Option<bool>,
    /// A route pattern to publish under.
    #[serde(default)]
    pub route: Option<String>,
    /// The zone the route belongs to.
    #[serde(default)]
    pub zone_id: Option<String>,
    /// `[cloudflare.vars]` — plaintext environment variables. Use secrets for anything sensitive.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// `[cloudflare.triggers]` — cron schedules.
    #[serde(default)]
    pub triggers: CfTriggers,
    /// `[cloudflare.handlers]` — Worker handlers that no other manifest key implies.
    #[serde(default)]
    pub handlers: CfHandlers,
    /// `[cloudflare.service.<name>]` — Cloudflare wiring for a portable `[[service]]`.
    #[serde(default)]
    pub service: BTreeMap<String, CloudflareServiceSection>,
    /// `[cloudflare.database.<name>]` — Cloudflare wiring for a portable `[[database]]`.
    #[serde(default)]
    pub database: BTreeMap<String, CloudflareDatabaseSection>,
    /// `[[cloudflare.kv_namespaces]]`.
    #[serde(default)]
    pub kv_namespaces: Vec<CfKvNamespace>,
    /// `[[cloudflare.r2_buckets]]`.
    #[serde(default)]
    pub r2_buckets: Vec<CfR2Bucket>,
    /// `[[cloudflare.d1_databases]]`.
    #[serde(default)]
    pub d1_databases: Vec<CfD1Database>,
    /// `[cloudflare.queues]`.
    #[serde(default)]
    pub queues: CfQueues,
    /// `[cloudflare.durable_objects]`.
    #[serde(default)]
    pub durable_objects: CfDurableObjects,
    /// `[[cloudflare.services]]` — service bindings to other Workers.
    #[serde(default)]
    pub services: Vec<CfServiceBinding>,
    /// `[cloudflare.assets]` — static assets served alongside the Worker.
    #[serde(default)]
    pub assets: Option<CfAssets>,
    /// `[[cloudflare.secrets_store_secrets]]`.
    #[serde(default)]
    pub secrets_store_secrets: Vec<CfSecretsStoreSecret>,
    /// `[cloudflare.raw]` — an escape hatch merged verbatim into the generated `wrangler.toml`.
    ///
    /// Anything wrangler accepts but Skyzen does not model goes here. The table is deep-merged
    /// over the rendered configuration by [`crate::deep_merge`], so it can both add new keys and
    /// override rendered ones.
    #[serde(default)]
    pub raw: toml::Table,
}

/// `[cloudflare.triggers]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfTriggers {
    /// Cron expressions that invoke the `scheduled` handler.
    #[serde(default)]
    pub crons: Vec<String>,
}

/// `[cloudflare.handlers]` — Worker handlers with no config-derived signal.
///
/// A `queue` handler is implied by a queue consumer and a `scheduled` handler by a cron trigger,
/// so neither appears here. Email routing and tail consumers are configured on the *sending*
/// side (in the dashboard, or in another Worker's `tail_consumers`), which leaves this Worker's
/// own manifest with nothing to infer from — hence the explicit opt-in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfHandlers {
    /// Export an `email` handler, implemented in Rust with `#[skyzen::email]`.
    #[serde(default)]
    pub email: bool,
    /// Export a `tail` handler, implemented in Rust with `#[skyzen::tail]`.
    #[serde(default)]
    pub tail: bool,
}

/// `[cloudflare.service.<name>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareServiceSection {
    /// The Cloudflare binding name the backend resolves through.
    pub binding: String,
}

/// `[cloudflare.database.<name>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareDatabaseSection {
    /// The D1 binding name the backend resolves through.
    pub binding: String,
}

/// One `[[cloudflare.kv_namespaces]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfKvNamespace {
    /// Binding name, as seen by `CfKv::from_env`.
    pub binding: String,
    /// The namespace id. Leave unset and run `skyzen provision` to have one created.
    #[serde(default)]
    pub id: Option<String>,
    /// A separate namespace id used by `wrangler dev --remote`.
    #[serde(default)]
    pub preview_id: Option<String>,
}

/// One `[[cloudflare.r2_buckets]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfR2Bucket {
    /// Binding name, as seen by `CfR2::from_env`.
    pub binding: String,
    /// The bucket name. R2 buckets are addressed by name, so there is no separate id.
    pub bucket_name: String,
    /// A separate bucket used by `wrangler dev --remote`.
    #[serde(default)]
    pub preview_bucket_name: Option<String>,
}

/// One `[[cloudflare.d1_databases]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfD1Database {
    /// Binding name, as seen by `CfD1::from_env`.
    pub binding: String,
    /// The database name, which is what `wrangler d1 create` is given.
    pub database_name: String,
    /// The database id. Leave unset and run `skyzen provision` to have one created.
    #[serde(default)]
    pub database_id: Option<String>,
    /// A separate database used by `wrangler dev --remote`.
    #[serde(default)]
    pub preview_database_id: Option<String>,
}

/// `[cloudflare.queues]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfQueues {
    /// `[[cloudflare.queues.producers]]` — queues this Worker sends to.
    #[serde(default)]
    pub producers: Vec<CfQueueProducer>,
    /// `[[cloudflare.queues.consumers]]` — queues this Worker's `queue` handler receives from.
    #[serde(default)]
    pub consumers: Vec<CfQueueConsumer>,
}

/// One `[[cloudflare.queues.producers]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfQueueProducer {
    /// Binding name, as seen by `CfQueue::from_env`.
    pub binding: String,
    /// The queue name.
    pub queue: String,
    /// Seconds to hold every message before it becomes visible to consumers.
    #[serde(default)]
    pub delivery_delay: Option<u32>,
}

/// One `[[cloudflare.queues.consumers]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfQueueConsumer {
    /// The queue name to consume from.
    pub queue: String,
    /// Most messages to deliver in one `queue` invocation.
    #[serde(default)]
    pub max_batch_size: Option<u32>,
    /// Seconds to wait for a batch to fill before delivering a partial one.
    #[serde(default)]
    pub max_batch_timeout: Option<u32>,
    /// Times a message is retried before it is dropped or dead-lettered.
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Queue that exhausted messages are forwarded to.
    #[serde(default)]
    pub dead_letter_queue: Option<String>,
    /// Most consumer invocations Cloudflare runs concurrently.
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// Seconds to hold a retried message before redelivering it.
    #[serde(default)]
    pub retry_delay: Option<u32>,
}

/// `[cloudflare.durable_objects]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfDurableObjects {
    /// `[[cloudflare.durable_objects.bindings]]`.
    #[serde(default)]
    pub bindings: Vec<CfDurableBinding>,
    /// `[[cloudflare.durable_objects.migrations]]`, rendered as wrangler's top-level
    /// `[[migrations]]`.
    #[serde(default)]
    pub migrations: Vec<CfDurableMigration>,
}

/// One `[[cloudflare.durable_objects.bindings]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfDurableBinding {
    /// Binding name, as seen from `env`.
    pub name: String,
    /// The Rust struct name of the `#[skyzen::durable_object]` type (e.g. `Room`). The macro
    /// exports the class as `{struct}Object` from the wasm bindings and the generated worker
    /// shim re-exports it under this name, which is also what wrangler sees. Do NOT append
    /// `Object` yourself.
    pub class_name: String,
    /// The Worker that defines the class, when it is not this one.
    #[serde(default)]
    pub script_name: Option<String>,
}

/// One `[[cloudflare.durable_objects.migrations]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfDurableMigration {
    /// Migration tag; bump it whenever the set of classes changes.
    pub tag: String,
    /// Classes to create with key/value storage.
    #[serde(default)]
    pub new_classes: Vec<String>,
    /// Classes to create with SQLite-backed storage.
    #[serde(default)]
    pub new_sqlite_classes: Vec<String>,
    /// Classes to delete.
    #[serde(default)]
    pub deleted_classes: Vec<String>,
    /// Classes to rename.
    #[serde(default)]
    pub renamed_classes: Vec<CfDurableRenamedClass>,
}

/// One entry of a migration's `renamed_classes`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfDurableRenamedClass {
    /// The current class name.
    pub from: String,
    /// The new class name.
    pub to: String,
}

/// One `[[cloudflare.services]]` entry — a binding to another Worker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfServiceBinding {
    /// Binding name, as seen from `env`.
    pub binding: String,
    /// The name of the Worker being bound.
    pub service: String,
    /// The named environment of that Worker to bind to.
    #[serde(default)]
    pub environment: Option<String>,
}

/// `[cloudflare.assets]` — static assets uploaded with the Worker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfAssets {
    /// Directory of files to upload, relative to the project root.
    pub directory: String,
    /// Binding name for programmatic access to the assets from the Worker.
    #[serde(default)]
    pub binding: Option<String>,
    /// What to serve for a request that matches no asset.
    #[serde(default)]
    pub not_found_handling: Option<CfAssetsNotFoundHandling>,
    /// Whether the Worker runs before the asset server rather than after it.
    #[serde(default)]
    pub run_worker_first: Option<bool>,
}

/// The `not_found_handling` strategies wrangler accepts for static assets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum CfAssetsNotFoundHandling {
    /// Fall through to the Worker.
    #[serde(rename = "none")]
    None,
    /// Serve `404.html` with a 404 status.
    #[serde(rename = "404-page")]
    NotFoundPage,
    /// Serve `index.html` with a 200 status.
    #[serde(rename = "single-page-application")]
    SinglePageApplication,
}

impl CfAssetsNotFoundHandling {
    /// The wrangler spelling of this strategy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NotFoundPage => "404-page",
            Self::SinglePageApplication => "single-page-application",
        }
    }
}

/// One `[[cloudflare.secrets_store_secrets]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CfSecretsStoreSecret {
    /// Binding name, as seen from `env`.
    pub binding: String,
    /// The secrets store the secret lives in.
    pub store_id: String,
    /// The secret's name within that store.
    pub secret_name: String,
}
