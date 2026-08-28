//! Portable service abstractions for the Skyzen framework.
//!
//! This crate defines platform-agnostic traits and extractors for common
//! backend services: key-value stores, object storage, message queues, and databases.
//!
//! Each service follows a two-layer design:
//! - A **public trait** (e.g. [`KeyValueStore`]) that is ergonomic for implementors
//! - A **wrapper struct** (e.g. [`Kv`]) that provides type-erased dynamic dispatch, and implements
//!   both [`skyzen_core::Extractor`] (a handler names it as an argument) and
//!   [`skyzen_core::middleware::Middleware`] (attaching it injects it)
//!
//! Portable SQL support is provided through [`Db`], including transactions where the backend has
//! them and [`Db::migrate`] everywhere. Rows are decoded by the type they are fetched into, one
//! column per field — see [`sql::FromRow`] and [`sql::FromColumn`].
//!
//! Three modules sit alongside the four capabilities:
//!
//! - [`migrate`] — the portable migration runner, its bookkeeping table and its checksum policy.
//! - [`queue::envelope`] — the in-band encoding for queue transports that carry only text, owned
//!   here because two crates that must not depend on each other both speak it.
//! - [`durable`] — [`DurableKv`](durable::DurableKv), [`DurableDb`](durable::DurableDb) and
//!   [`Alarm`](durable::Alarm), the object-scoped storage a Durable Object gets.
//!
//! # Example
//!
//! ```ignore
//! use skyzen::FromRow;
//! use skyzen_services::{Db, Kv, Storage};
//!
//! /// One field per selected column, decoded by the field's own type — see [`sql::FromRow`].
//! #[derive(FromRow, Serialize)]
//! struct User {
//!     id: Uuid,
//!     email: String,
//! }
//!
//! async fn handler(kv: Kv, db: Db) -> Result<Json<Vec<User>>> {
//!     kv.put_json("cache:key", &json!({"hello": "world"})).await?;
//!     let users: Vec<User> = db.query("SELECT id, email FROM users").fetch_all().await?;
//!     Ok(Json(users))
//! }
//! ```

// Note: the native driver features (`postgres`, `mysql`, `sqlite`) are inert on
// wasm32 targets — sqlx is a non-wasm dependency, and all driver-backed code in
// `sql.rs` is additionally gated on `not(target_arch = "wasm32")`. On wasm,
// use cloud vendor database services instead (for Cloudflare, use
// skyzen-cloudflare::CfD1 or skyzen_services::durable::DurableDb).

#[macro_use]
mod macros;

/// The underlying cause carried by a service error's `Backend` variant.
///
/// Backends box their own SDK error here so `source()` still reaches it after the message has
/// been rendered for the client.
pub type BoxError = Box<dyn core::error::Error + Send + Sync + 'static>;

/// Boxed future returned by the object-safe mirror of each service trait.
///
/// Service futures are always `Send`, on every target: the wrappers travel in
/// [`http::Extensions`], which requires `Send + Sync` unconditionally. Single-threaded WebAssembly
/// backends satisfy that by wrapping their JS handles in newtypes with a contained `unsafe impl
/// Send` — sound because a Workers isolate never moves them across threads — not by relaxing the
/// bound here.
pub(crate) type BoxFuture<'a, T> = futures_core::future::BoxFuture<'a, T>;

pub mod durable;
pub mod events;
pub mod kv;
pub mod migrate;
pub mod queue;
pub mod sql;
pub mod storage;

/// The SQL tokenizer every Skyzen backend rewrites statements with.
///
/// Re-exported because [`sql::tokenize_with_dialect`] and [`sql::LocationMapper`] speak in
/// sqlparser's own types: a platform crate doing a second rewriting pass (the Aurora Data API
/// turns Skyzen's placeholders into its named form) has to name the *same* `Dialect` and `Token`
/// types, and depending on sqlparser separately only works for as long as the two versions happen
/// to agree.
pub use sqlparser;

/// The JSON implementation a row's portable form is written in.
///
/// Re-exported for the same reason [`sqlparser`] is: [`sql::FromColumn`] is written against
/// `serde_json::Value`, so a macro expansion or a backend crate has to be able to name the *same*
/// `Value` type, not merely one from a version that happens to agree.
pub use serde_json;

pub use durable::{DurableDb, DurableDbBackend, DurableDbError, DurableDbQuery};
pub use events::ScheduledTick;
pub use kv::{KeyValueStore, Kv, KvError, KvListOptions, KvListResult};
pub use migrate::{AppliedMigration, Migration, MigrationReport, MigrationStatus, Migrations};
pub use queue::{
    BatchSendFailure, MessageQueue, MessageReceipt, Queue, QueueBatch, QueueBatchDisposition,
    QueueError, QueueMessage, QueueMessageDisposition, QueueRetry, ReceiveOptions, ReceivedMessage,
    SendOptions,
};
pub use sql::{
    BatchStatement, ColumnEnum, ColumnError, Db, DbBackend, DbDialect, DbError, DbExecResult,
    DbQuery, DbTransaction, DbTransactionBackend, DbTransactionQuery, DbValue, FromColumn, FromRow,
    JsonRow, LocationMapper, QuerySource, Row, RowError, SqlQuery,
};
pub use storage::{
    ByteRange, ListOptions, ListResult, ObjectMetadata, ObjectStorage, PresignedRequest, PutOption,
    PutOptions, Storage, StorageError, StorageObject, StorageStream,
};

#[cfg(test)]
mod http_status_tests {
    use super::{
        durable::{AlarmError, DurableKvError},
        BatchSendFailure, BoxError, DbError, DurableDbError, KvError, QueueError, StorageError,
    };
    use core::error::Error as StdError;
    use http_kit::{HttpError, StatusCode};

    fn cause() -> BoxError {
        Box::new(std::io::Error::other("connection reset"))
    }

    fn assert_statuses(cases: &[(&dyn HttpError, StatusCode)]) {
        for (error, expected) in cases {
            assert_eq!(error.status(), *expected, "unexpected status for {error}");
        }
    }

    #[test]
    fn kv_error_statuses() {
        assert_statuses(&[
            (
                &KvError::backend("get failed"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                &KvError::Decode("not utf-8".to_owned()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (&KvError::Unsupported("ttl"), StatusCode::NOT_IMPLEMENTED),
            (&KvError::Conflict, StatusCode::CONFLICT),
            (
                &KvError::Throttled { retry_after: None },
                StatusCode::TOO_MANY_REQUESTS,
            ),
            (&KvError::Unauthorized, StatusCode::INTERNAL_SERVER_ERROR),
        ]);
    }

    #[test]
    fn storage_error_statuses() {
        assert_statuses(&[
            (
                &StorageError::backend("put failed"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                &StorageError::Io("short read".to_owned()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                &StorageError::Unsupported("multipart"),
                StatusCode::NOT_IMPLEMENTED,
            ),
            (&StorageError::Conflict, StatusCode::CONFLICT),
            (
                &StorageError::Throttled { retry_after: None },
                StatusCode::TOO_MANY_REQUESTS,
            ),
            (
                &StorageError::Unauthorized,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ]);
    }

    #[test]
    fn queue_error_statuses() {
        assert_statuses(&[
            (
                &QueueError::backend("send failed"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                &QueueError::Unsupported("receive"),
                StatusCode::NOT_IMPLEMENTED,
            ),
            (&QueueError::Conflict, StatusCode::CONFLICT),
            (
                &QueueError::Throttled { retry_after: None },
                StatusCode::TOO_MANY_REQUESTS,
            ),
            (&QueueError::Unauthorized, StatusCode::INTERNAL_SERVER_ERROR),
            (
                &QueueError::PartialBatch {
                    failures: vec![BatchSendFailure {
                        index: 3,
                        code: "InternalError".to_owned(),
                        message: "something broke".to_owned(),
                    }],
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ]);
    }

    #[test]
    fn db_error_statuses() {
        assert_statuses(&[
            (
                &DbError::backend("query failed"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                &DbError::ParameterCountMismatch {
                    expected: 2,
                    actual: 1,
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                &DbError::SqlParse("unbalanced quote".to_owned()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (&DbError::RowNotFound, StatusCode::NOT_FOUND),
            (
                &DbError::TransactionsUnsupported,
                StatusCode::NOT_IMPLEMENTED,
            ),
            (&DbError::Conflict, StatusCode::CONFLICT),
            (
                &DbError::Throttled { retry_after: None },
                StatusCode::TOO_MANY_REQUESTS,
            ),
            // A rejected credential is a deployment fault, so it renders as a 500 exactly like the
            // key-value, storage and queue errors do — never as a 401 aimed at the HTTP caller.
            (&DbError::Unauthorized, StatusCode::INTERNAL_SERVER_ERROR),
            (
                &DbError::MigrationChanged {
                    version: 1,
                    name: "create_users".to_owned(),
                    recorded: "aa".repeat(32),
                    embedded: "bb".repeat(32),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ]);
    }

    #[test]
    fn an_edited_migration_names_the_file_and_both_checksums() {
        let error = DbError::MigrationChanged {
            version: 7,
            name: "add_index".to_owned(),
            recorded: "aa".repeat(32),
            embedded: "bb".repeat(32),
        };
        let rendered = error.to_string();
        for fragment in ["add_index", "version 7", &"aa".repeat(32), &"bb".repeat(32)] {
            assert!(
                rendered.contains(fragment),
                "{fragment} missing: {rendered}"
            );
        }
    }

    #[test]
    fn durable_error_statuses() {
        assert_statuses(&[
            (
                &DurableKvError::backend("storage failed"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (&DurableDbError::RowNotFound, StatusCode::NOT_FOUND),
            (
                &DurableDbError::backend("sql failed"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                &AlarmError::backend("alarm failed"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ]);
    }

    #[test]
    fn backend_with_keeps_the_underlying_cause() {
        let error = KvError::backend_with("get `user:1` failed", cause());
        assert_eq!(error.to_string(), "kv store error: get `user:1` failed");
        assert_eq!(
            StdError::source(&error)
                .expect("backend_with records a source")
                .to_string(),
            "connection reset"
        );
    }

    #[test]
    fn backend_without_a_cause_has_no_source() {
        assert!(StdError::source(&KvError::backend("get failed")).is_none());
    }

    #[test]
    fn durable_db_error_conversion_forwards_the_cause() {
        let error = DurableDbError::from(DbError::backend_with("insert failed", cause()));
        assert_eq!(error.to_string(), "durable database error: insert failed");
        assert_eq!(
            StdError::source(&error)
                .expect("conversion keeps the source")
                .to_string(),
            "connection reset"
        );
    }
}
