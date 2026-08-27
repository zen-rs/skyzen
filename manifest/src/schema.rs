//! The typed `Skyzen.toml` schema.
//!
//! Every struct carries `#[serde(deny_unknown_fields)]`: a mistyped key is a hard error rather
//! than a silently dropped binding. String-valued discriminants (`type`, `backend`) are modelled
//! as enums so an unsupported value is rejected by the parser instead of by a `match` arm buried
//! in a consumer.

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroUsize},
    time::Duration,
};

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
    /// `[aws]` — AWS Lambda deployment configuration.
    #[serde(default)]
    pub aws: Option<AwsSection>,
    /// `[azure]` — Azure Functions deployment configuration.
    #[serde(default)]
    pub azure: Option<AzureSection>,
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
    /// `[[native.queue_consumer]]` — queues this application consumes natively.
    ///
    /// The Cloudflare counterpart is `[[cloudflare.queues.consumers]]`: there the platform pushes
    /// batches into the Worker, whereas natively Skyzen runs the polling loop itself. Both drive
    /// the same `#[skyzen::queue]` handler.
    #[serde(default)]
    pub queue_consumer: Vec<NativeQueueConsumer>,
}

/// One `[[native.queue_consumer]]` entry — a polling loop over one portable queue.
///
/// Durations are humantime strings (`"20s"`, `"1m 30s"`, `"500ms"`) and are parsed here, so a
/// malformed one fails the manifest parse rather than surfacing as a runtime surprise inside the
/// consumer loop.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeQueueConsumer {
    /// The `[[service]]` name to consume from. It must name an entry of `type = "queue"`.
    pub service: String,
    /// How many polling loops run against this queue at once.
    ///
    /// Each loop receives, invokes the handler and settles independently, so this is the
    /// application's own concurrency limit for the queue — the native counterpart of Cloudflare's
    /// `max_concurrency`.
    #[serde(default = "default_concurrency")]
    pub concurrency: NonZeroUsize,
    /// Most messages to take in one receive. Backends cap this at their own batch size (SQS
    /// allows 10, Azure Storage 32), so a larger value is not an error, just a smaller batch.
    #[serde(default = "default_batch_size")]
    pub batch_size: NonZeroUsize,
    /// How long a receive may wait for a message before returning empty (long polling).
    ///
    /// This is also the loop's idle pace: a backend that answers an empty receive sooner than
    /// this leaves the loop idle for the remainder rather than spinning on it.
    #[serde(default = "default_poll_wait", with = "humantime_serde")]
    pub poll_wait: Duration,
    /// How long a received batch stays invisible to other consumers. Unset uses the queue's own
    /// configured default.
    #[serde(default, with = "humantime_serde")]
    pub visibility_timeout: Option<Duration>,
    /// How long a retried message is held before redelivery, when the handler asks for a retry
    /// without naming its own delay.
    #[serde(default = "default_retry_delay", with = "humantime_serde")]
    pub retry_delay: Duration,
}

/// One polling loop per queue, which is the only concurrency an application has not asked for.
const fn default_concurrency() -> NonZeroUsize {
    NonZeroUsize::new(1).expect("1 is not zero")
}

/// Ten messages per receive — SQS's maximum, and at or under every other backend's.
const fn default_batch_size() -> NonZeroUsize {
    NonZeroUsize::new(10).expect("10 is not zero")
}

/// Twenty seconds of long polling, which is SQS's maximum `WaitTimeSeconds`.
const fn default_poll_wait() -> Duration {
    Duration::from_secs(20)
}

/// Thirty seconds before a retried message comes back, matching SQS's default visibility timeout.
const fn default_retry_delay() -> Duration {
    Duration::from_secs(30)
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

/// `[aws]` — AWS Lambda deployment configuration.
///
/// The same `#[skyzen::main]` binary serves every target, so this section carries only what the
/// *deployment* needs: what the function is called, how big it is, and what environment it runs
/// with. Nothing here reaches the application at compile time.
///
/// Unlike `[cloudflare]`, this section has no named environment overlays: `[cloudflare.env.<name>]`
/// exists because wrangler models environments itself, and neither `cargo lambda` nor the Functions
/// host does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsSection {
    /// The Lambda function's name. Defaults to the binary's own name, which is what
    /// `cargo lambda deploy` uses when it is given none.
    #[serde(default)]
    pub function_name: Option<String>,
    /// Memory to allocate, in MB. Lambda scales CPU with it, so this is also the speed dial.
    #[serde(default)]
    pub memory_mb: Option<NonZeroU32>,
    /// How long one invocation may run before Lambda kills it (humantime, e.g. `"30s"`).
    #[serde(default, with = "humantime_serde")]
    pub timeout: Option<Duration>,
    /// The instruction set to build and deploy for.
    #[serde(default)]
    pub architecture: LambdaArchitecture,
    /// `[aws.env]` — plaintext environment variables set on the function.
    ///
    /// These are visible in the console and in `GetFunctionConfiguration`; anything sensitive
    /// belongs in Secrets Manager or SSM, read by the application at startup.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Whether the deployment creates (and keeps) a Lambda Function URL.
    ///
    /// Defaults to `true`: a Skyzen application is an HTTP server, and without a URL — or an API
    /// Gateway wired up by hand — nothing can reach it.
    #[serde(default = "default_true")]
    pub url: bool,
}

/// A `true` that serde can name as a field default.
const fn default_true() -> bool {
    true
}

/// The instruction sets AWS Lambda runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
pub enum LambdaArchitecture {
    /// Graviton. The default: cheaper per millisecond than `x86_64` at the same memory size.
    #[default]
    #[serde(rename = "arm64")]
    Arm64,
    /// Intel/AMD, for a dependency that has no aarch64 build.
    #[serde(rename = "x86_64")]
    X86_64,
}

impl LambdaArchitecture {
    /// The manifest spelling of this architecture.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::X86_64 => "x86_64",
        }
    }

    /// The Rust target triple `cargo lambda build` cross-compiles to.
    ///
    /// Named outright rather than left to `cargo lambda`'s default so the architecture the
    /// manifest declares is the architecture that is built, whichever machine builds it.
    #[must_use]
    pub const fn target_triple(self) -> &'static str {
        match self {
            Self::Arm64 => "aarch64-unknown-linux-gnu",
            Self::X86_64 => "x86_64-unknown-linux-gnu",
        }
    }
}

/// `[azure]` — Azure Functions deployment configuration.
///
/// Skyzen deploys to Functions as a [custom handler]: the Functions host runs the compiled binary
/// as a web server and forwards events to it over HTTP. HTTP-trigger functions arrive as ordinary
/// requests that the application's own router answers; a queue trigger arrives as the custom
/// handler envelope sent by POST to the function's name, which `queue_triggers` declares.
///
/// [custom handler]: https://learn.microsoft.com/azure/azure-functions/functions-custom-handlers
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AzureSection {
    /// The Function App to publish to, as `func azure functionapp publish` names it.
    #[serde(default)]
    pub app_name: Option<String>,
    /// The Rust target triple to build the handler for.
    ///
    /// A Function App runs Linux, so a handler built on macOS or Windows cannot be published as
    /// it is. Set this to a Linux triple (`x86_64-unknown-linux-musl` needs no cross toolchain
    /// beyond `rustup target add`) and the CLI builds through it.
    #[serde(default)]
    pub target: Option<String>,
    /// How the Functions host hands an HTTP-trigger request to the handler.
    #[serde(default)]
    pub http_mode: AzureHttpMode,
    /// `[[azure.queue_triggers]]` — Storage queue triggers the host drives.
    #[serde(default)]
    pub queue_triggers: Vec<AzureQueueTrigger>,
}

/// How the Functions host delivers an HTTP-trigger request to a custom handler.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AzureHttpMode {
    /// `enableForwardingHttpRequest` — the host forwards the request and buffers the response.
    ///
    /// The default, and the mode the Functions documentation covers most thoroughly.
    #[default]
    Forward,
    /// `enableProxyingHttpRequest` — the host proxies the request and streams the response back.
    ///
    /// Pick this for an application that streams: server-sent events and chunked responses are
    /// held until completion under `forward`, which turns a live event stream into one long
    /// silence.
    Proxy,
}

impl AzureHttpMode {
    /// The `host.json` key under `customHandler` that turns this mode on.
    #[must_use]
    pub const fn host_json_key(self) -> &'static str {
        match self {
            Self::Forward => "enableForwardingHttpRequest",
            Self::Proxy => "enableProxyingHttpRequest",
        }
    }
}

/// One `[[azure.queue_triggers]]` entry — a Storage queue the Functions host drives.
///
/// The host owns the polling loop and POSTs each message to the handler, so this is the Azure
/// counterpart of `[[native.queue_consumer]]` (where Skyzen polls) and of
/// `[[cloudflare.queues.consumers]]` (where Cloudflare pushes). All three drive the one
/// `#[skyzen::queue]` handler.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AzureQueueTrigger {
    /// The Function name, which is both the generated `function.json` directory and the URL path
    /// the host POSTs the message to. It must not collide with a route the application serves.
    pub function: String,
    /// The Storage queue to trigger on, as `function.json`'s `queueName`.
    pub queue: String,
    /// The application setting holding the storage connection, as `function.json`'s `connection`.
    /// `AzureWebJobsStorage` is the Function App's own storage account.
    pub connection_env: String,
}

/// The Functions name Skyzen gives the catch-all HTTP function.
///
/// The whole application is served through this one function, so a queue trigger claiming the name
/// would take the directory its `function.json` is written into — and the deployment would come up
/// with no HTTP surface at all.
pub const HTTP_FUNCTION_NAME: &str = "http";

/// Why a `[[azure.queue_triggers]]` entry cannot be used.
///
/// Checked when the manifest is parsed, so `#[skyzen::main]` reports it as a compile error and the
/// CLI reports it before generating anything — one rule, both consumers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueTriggerError {
    /// The name is not one Azure would accept, and is also a path the CLI would write into.
    #[error(
        "`{function}` is not a valid Functions name: it must start with a letter and contain only \
         letters, digits, hyphens and underscores"
    )]
    Name {
        /// The rejected name.
        function: String,
    },
    /// The name is the one the catch-all HTTP function already uses.
    #[error(
        "`{function}` is the name Skyzen gives the catch-all HTTP function; a queue trigger with \
         that name would replace it, leaving the application with no HTTP surface"
    )]
    Reserved {
        /// The rejected name.
        function: String,
    },
    /// Two entries claim one name, and therefore one directory and one URL path.
    #[error("the Functions name `{function}` is declared twice; each one is a separate function")]
    Duplicate {
        /// The name declared more than once.
        function: String,
    },
}

impl AzureQueueTrigger {
    /// Check that this entry names a function Azure would accept and Skyzen has not taken.
    ///
    /// # Errors
    ///
    /// [`QueueTriggerError`] when the name is malformed or reserved.
    pub fn validate(&self) -> Result<(), QueueTriggerError> {
        let mut characters = self.function.chars();
        let well_formed = characters.next().is_some_and(char::is_alphabetic)
            && characters
                .all(|character| character.is_alphanumeric() || matches!(character, '-' | '_'));
        if !well_formed {
            return Err(QueueTriggerError::Name {
                function: self.function.clone(),
            });
        }

        // Compared case-insensitively because the bundle is a directory tree, and a
        // case-insensitive filesystem (macOS, Windows) would collide where a comparison of the
        // bytes would not.
        if self.function.eq_ignore_ascii_case(HTTP_FUNCTION_NAME) {
            return Err(QueueTriggerError::Reserved {
                function: self.function.clone(),
            });
        }

        Ok(())
    }
}

impl AzureSection {
    /// Check every declared queue trigger, and that no two claim the same name.
    ///
    /// # Errors
    ///
    /// [`QueueTriggerError`] for the first entry that cannot be used.
    pub fn validate_queue_triggers(&self) -> Result<(), QueueTriggerError> {
        let mut claimed = std::collections::BTreeSet::new();
        for trigger in &self.queue_triggers {
            trigger.validate()?;
            if !claimed.insert(trigger.function.to_ascii_lowercase()) {
                return Err(QueueTriggerError::Duplicate {
                    function: trigger.function.clone(),
                });
            }
        }
        Ok(())
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
