//! Amazon RDS Data API implementation of [`DbBackend`].
//!
//! The Data API is an HTTP endpoint in front of an Aurora cluster: every statement is one signed
//! request, authenticated with a Secrets Manager secret, with no connection to open and no pool to
//! keep warm. That is what makes it the SQL backend for runtimes that cannot hold a socket — a
//! Lambda that may be frozen between invocations, or a worker with no VPC attachment — and it is
//! also the first serverless backend in Skyzen offering **real interactive transactions**
//! ([`DbBackend::begin`]), which Cloudflare D1 cannot.
//!
//! # Placeholders are rewritten twice
//!
//! A caller writes `?` on every dialect. [`Db`](skyzen_services::Db) rewrites those into the
//! backend's native form before calling this crate — `$1`, `$2`, … for `PostgreSQL`, `?` for
//! `MySQL` — and the Data API accepts *neither*: it binds only named parameters, `:name` in the SQL
//! paired with a `SqlParameter` list. So this backend runs a second rewriting pass, turning the
//! dialect-native placeholders into `:p1` … `:pN`.
//!
//! That pass runs on `sqlparser`'s tokenizer — the same crate and the same dialects
//! `skyzen-services` uses — so a `?` or a `$1` inside a string literal, a dollar-quoted body, a
//! backtick-quoted identifier, a `--` comment or a `/* */` comment is left exactly as written. A
//! textual search-and-replace would corrupt all five.
//!
//! # Binding: values and type hints
//!
//! The Data API's `Field` union carries a boolean, a long, a double, a string, a blob, or a null,
//! and nothing else. Everything richer travels as a string plus a `typeHint` that tells the service
//! which database type to cast it to:
//!
//! | [`DbValue`] | Field | Type hint |
//! | --- | --- | --- |
//! | `Null` | `isNull` | — |
//! | `Boolean` | `booleanValue` | — |
//! | `Integer` | `longValue` | — |
//! | `Real` | `doubleValue` | — |
//! | `Text` | `stringValue` | — |
//! | `Blob` | `blobValue` | — |
//! | `Timestamp` | `stringValue`, `YYYY-MM-DD HH:MM:SS.ffffff` | `TIMESTAMP` |
//! | `Uuid` (Aurora `PostgreSQL`) | `stringValue`, hyphenated | `UUID` |
//! | `Uuid` (Aurora `MySQL`) | `blobValue`, 16 bytes | — |
//! | `Decimal` | `stringValue` | `DECIMAL` |
//! | `Json` | `stringValue` | `JSON` |
//!
//! Three of those rows deserve their reasoning spelled out:
//!
//! - **Timestamps are rendered in UTC with microsecond precision.** AWS documents the accepted
//!   format as `YYYY-MM-DD HH:MM:SS[.FFF]`, but three fractional digits would silently truncate
//!   values both engines store to microsecond precision, so six are sent: a service that rejected
//!   them would fail loudly rather than quietly rounding a caller's data. The rendering carries no
//!   zone, so a `timestamptz` column applies the session time zone — UTC on an unmodified cluster,
//!   which is what the value was rendered in.
//! - **A UUID binds as 16 bytes on Aurora `MySQL`**, not as a hinted string, because that is what
//!   the sqlx `MySQL` backend binds and `MySQL` has no UUID type; the same application code has to
//!   read the same rows against either backend. On Aurora `PostgreSQL` the `UUID` hint is exact.
//! - **`Decimal` binds as a hinted string**, which is exact — unlike a double — and is why
//!   [`DbValue::Decimal`] does not have to be stringified by the caller.
//!
//! # Rows, and what the Data API loses on the way back
//!
//! Rows come back as `records` plus `columnMetadata` and are converted into the same JSON row shape
//! every other backend produces: one object per row, keyed by the column's label (its `AS` alias
//! when it has one) and falling back to its name. Blobs become arrays of byte values, `NaN` and
//! infinity become `null`, and `PostgreSQL` arrays become JSON arrays. Every statement pins
//! `resultSetOptions` to `DECIMAL → STRING` and `LONG → LONG`, so a `NUMERIC` column arrives exact
//! as a string instead of being rounded through a double, matching the contract documented on
//! [`DbExecResult::rows`].
//!
//! Two differences from the sqlx backends are real and cannot be papered over here:
//!
//! - **Dates, times and timestamps arrive in the Data API's own textual rendering**, not RFC 3339.
//!   A `DateTime<Utc>` field will therefore fail to deserialize where it would have succeeded
//!   against sqlx `PostgreSQL`; type the field as `chrono::NaiveDateTime`, or render it yourself
//!   with `to_char(...)` in the query. This backend passes the service's string through untouched
//!   rather than guessing at a format and re-rendering it, because a wrong guess would silently
//!   shift timestamps instead of failing.
//! - **`generatedFields` is ignored.** Aurora `PostgreSQL` does not populate it at all — use
//!   `INSERT … RETURNING`, which the row path already returns — and on Aurora `MySQL` an
//!   auto-increment id needs a follow-up `SELECT LAST_INSERT_ID()`.
//!
//! # Transactions and batches
//!
//! [`DbBackend::begin`] issues a real `BeginTransaction` and hands back an
//! [`RdsDataTransaction`] that carries the transaction id into every statement it runs, ending in
//! `CommitTransaction` or `RollbackTransaction`. A transaction that is dropped without either is
//! **not** closed by this crate: the service rolls it back on its own after three minutes without a
//! call, and until then the rows it touched stay locked. Commit or roll back explicitly.
//!
//! [`DbBackend::execute_batch`] runs `BeginTransaction`, the statements, then `CommitTransaction`,
//! rolling back on the first failure. It deliberately does **not** use the Data API's
//! `BatchExecuteStatement`, which is a different operation than its name suggests: it runs *one*
//! SQL string against *many* parameter sets, so it cannot express a batch of different statements,
//! and it returns no rows.
//!
//! # Limits worth designing around
//!
//! - **The response size limit is 1 MiB.** A query returning more is terminated by the service, so
//!   paginate with `LIMIT`/`OFFSET` rather than streaming a large table through this backend.
//! - **A statement call times out after 45 seconds.** Set
//!   [`with_continue_after_timeout`](RdsDataDb::with_continue_after_timeout) for DDL, which AWS
//!   recommends finishing server-side rather than being cut off mid-migration.
//! - **Only writer instances serve the Data API**, including for reads, and it is unavailable on
//!   `T` instance classes.
//! - The `schema` request field is not exposed here: AWS documents it as currently unsupported, so
//!   a setter for it would be a knob that silently does nothing. Qualify names in the SQL, or set
//!   `search_path` on the database user.
//!
//! # Errors
//!
//! Service error codes are classified before they become a [`DbError`], so a handler never has to
//! match on message text: throttling becomes [`DbError::Throttled`] and a permission rejection
//! (`ForbiddenException`, `AccessDeniedException`) becomes [`DbError::Unauthorized`], matching how
//! [`KvError`](skyzen_services::KvError) and [`QueueError`](skyzen_services::QueueError) report the
//! same rejection from `DynamoDB` and SQS. Everything else, including `BadRequestException` (which
//! is how a SQL error arrives), `StatementTimeoutException` and the transient
//! `DatabaseResumingException`, stays a [`DbError::Backend`] with the SDK's error as its source.

use std::str::FromStr;

use aws_sdk_rdsdata::operation::execute_statement::{
    builders::ExecuteStatementFluentBuilder, ExecuteStatementOutput,
};
use aws_sdk_rdsdata::primitives::Blob;
use aws_sdk_rdsdata::types::{
    ArrayValue, ColumnMetadata, DecimalReturnType, Field, LongReturnType, ResultSetOptions,
    SqlParameter, TypeHint,
};
use aws_sdk_rdsdata::Client;
use aws_smithy_types::error::display::DisplayErrorContext;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use serde_json::{Map, Number, Value};
// The tokenizer, the location-to-byte mapper and the `DbError::SqlParse` mapping all come from
// `skyzen-services`: this backend rewrites a statement that crate has already rewritten once, so
// re-deriving any of it here would be two copies of the same forward scan drifting apart. The
// sqlparser types come through the same re-export, which is what guarantees the two crates are
// talking about one `Dialect`.
use skyzen_services::{
    sql::{
        tokenize_with_dialect, BatchStatement, DbBackend, DbDialect, DbError, DbExecResult,
        DbTransaction, DbTransactionBackend, DbValue, LocationMapper,
    },
    sqlparser::{
        dialect::{MySqlDialect, PostgreSqlDialect},
        tokenizer::{Token, TokenWithSpan},
    },
};

use crate::errors::{categorize, AwsErrorCategory};

/// The environment variable holding the Aurora cluster ARN.
const RESOURCE_ARN_ENV: &str = "RDS_RESOURCE_ARN";

/// The environment variable holding the Secrets Manager secret ARN.
const SECRET_ARN_ENV: &str = "RDS_SECRET_ARN";

/// The environment variable holding the database name.
const DATABASE_ENV: &str = "RDS_DATABASE";

/// The environment variable holding the engine name.
const ENGINE_ENV: &str = "RDS_ENGINE";

/// How a [`DbValue::Timestamp`] is rendered for a `TIMESTAMP` type hint.
///
/// AWS documents `YYYY-MM-DD HH:MM:SS[.FFF]`; the six fractional digits here are the precision
/// Aurora `PostgreSQL` and Aurora `MySQL` actually store, and truncating to three would lose a
/// caller's data without saying so.
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.6f";

/// The Data API's own code for "these credentials may not make this call".
///
/// Other AWS services say this with `AccessDeniedException`, which [`categorize`] already knows;
/// the Data API adds this one, so it is classified here rather than widening the shared list that
/// `DynamoDB` and SQS also read.
const FORBIDDEN_CODE: &str = "ForbiddenException";

/// Which Aurora engine the Data API endpoint fronts.
///
/// The engine decides the SQL dialect the caller's statements are rewritten into, and it decides
/// how a [`DbValue::Uuid`] is bound, so it is settled when the backend is built rather than
/// discovered from an error on the first query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdsEngine {
    /// Aurora `PostgreSQL`, named `aurora-postgresql` by RDS.
    AuroraPostgres,
    /// Aurora `MySQL`, named `aurora-mysql` by RDS.
    AuroraMysql,
}

impl RdsEngine {
    /// The RDS engine identifier for Aurora `PostgreSQL`.
    const AURORA_POSTGRES: &'static str = "aurora-postgresql";

    /// The RDS engine identifier for Aurora `MySQL`.
    const AURORA_MYSQL: &'static str = "aurora-mysql";

    /// The SQL dialect statements for this engine are written in.
    #[must_use]
    pub const fn dialect(self) -> DbDialect {
        match self {
            Self::AuroraPostgres => DbDialect::Postgres,
            Self::AuroraMysql => DbDialect::MySql,
        }
    }
}

impl FromStr for RdsEngine {
    type Err = DbError;

    /// Parse an RDS engine identifier, case-insensitively.
    ///
    /// Only the two identifiers RDS itself uses are accepted. An unrecognized value is a
    /// deployment mistake that would otherwise surface as wrongly rewritten SQL, so it fails here.
    ///
    /// # Errors
    ///
    /// [`DbError::Backend`] naming both accepted values.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            Self::AURORA_POSTGRES => Ok(Self::AuroraPostgres),
            Self::AURORA_MYSQL => Ok(Self::AuroraMysql),
            other => Err(DbError::backend(format!(
                "unknown RDS engine `{other}`; expected `{}` or `{}`",
                Self::AURORA_POSTGRES,
                Self::AURORA_MYSQL
            ))),
        }
    }
}

/// An Aurora database reached through the RDS Data API.
///
/// See the [module documentation](self) for the placeholder rewriting, the type hints, the row
/// conversion and the service limits this backend works within.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Debug, Clone)]
pub struct RdsDataDb {
    client: Client,
    resource_arn: String,
    secret_arn: String,
    database: String,
    engine: RdsEngine,
    continue_after_timeout: bool,
}

impl RdsDataDb {
    /// Create an `RdsDataDb` from an existing client and the cluster it addresses.
    ///
    /// `resource_arn` is the Aurora cluster's ARN and `secret_arn` the Secrets Manager secret
    /// holding its credentials; `database` is the database to run statements against.
    #[must_use]
    pub fn new(
        client: Client,
        resource_arn: impl Into<String>,
        secret_arn: impl Into<String>,
        database: impl Into<String>,
        engine: RdsEngine,
    ) -> Self {
        Self {
            client,
            resource_arn: resource_arn.into(),
            secret_arn: secret_arn.into(),
            database: database.into(),
            engine,
            continue_after_timeout: false,
        }
    }

    /// Create an `RdsDataDb` from environment configuration.
    ///
    /// Reads `RDS_RESOURCE_ARN`, `RDS_SECRET_ARN`, `RDS_DATABASE` and `RDS_ENGINE`, and builds the
    /// client from the ambient AWS configuration the way every other backend in this crate does.
    ///
    /// # Errors
    ///
    /// [`DbError::Backend`] if any of the four variables is unset, or if `RDS_ENGINE` does not name
    /// an engine the Data API serves.
    pub async fn from_env() -> Result<Self, DbError> {
        let resource_arn = required_env(RESOURCE_ARN_ENV, "an Aurora cluster ARN")?;
        let secret_arn = required_env(SECRET_ARN_ENV, "a Secrets Manager secret ARN")?;
        let database = required_env(DATABASE_ENV, "a database name")?;
        let engine = required_env(ENGINE_ENV, "`aurora-postgresql` or `aurora-mysql`")?.parse()?;

        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Ok(Self::new(
            Client::new(&config),
            resource_arn,
            secret_arn,
            database,
            engine,
        ))
    }

    /// Let a statement keep running server-side after the Data API's 45-second call timeout.
    ///
    /// Off by default, matching the service. Turn it on for DDL: AWS recommends it there because a
    /// schema change cut off mid-flight can leave the catalog inconsistent, and the caller sees the
    /// timeout either way.
    #[must_use]
    pub const fn with_continue_after_timeout(mut self, continue_after_timeout: bool) -> Self {
        self.continue_after_timeout = continue_after_timeout;
        self
    }

    /// Which engine this backend was built for.
    #[must_use]
    pub const fn engine(&self) -> RdsEngine {
        self.engine
    }

    /// Build the `ExecuteStatement` request for one statement.
    ///
    /// Every statement this backend sends — plain, transactional, or part of a batch — is built
    /// here, so the parts that must never differ between those paths (the ARNs, the rewritten SQL,
    /// the named parameters, the pinned result-set options) cannot drift apart.
    fn statement(
        &self,
        sql: &str,
        params: &[DbValue],
        transaction_id: Option<&str>,
        rows: RowMode,
    ) -> Result<ExecuteStatementFluentBuilder, DbError> {
        let sql = rewrite_placeholders(sql, self.engine, params.len())?;
        let parameters = params
            .iter()
            .enumerate()
            .map(|(position, value)| sql_parameter(position + 1, value, self.engine))
            .collect::<Result<Vec<_>, DbError>>()?;

        Ok(self
            .client
            .execute_statement()
            .resource_arn(self.resource_arn.as_str())
            .secret_arn(self.secret_arn.as_str())
            .database(self.database.as_str())
            .sql(sql)
            .set_parameters(Some(parameters))
            .include_result_metadata(rows == RowMode::Collect)
            .continue_after_timeout(self.continue_after_timeout)
            .result_set_options(result_set_options())
            .set_transaction_id(transaction_id.map(ToOwned::to_owned)))
    }

    /// Send one statement and convert its response.
    async fn run(
        &self,
        sql: &str,
        params: &[DbValue],
        transaction_id: Option<&str>,
        rows: RowMode,
    ) -> Result<DbExecResult, DbError> {
        let output = self
            .statement(sql, params, transaction_id, rows)?
            .send()
            .await
            .map_err(|error| sdk_error("run a statement", error))?;
        exec_result(&output, rows)
    }

    /// Begin a transaction, returning the concrete backend rather than the erased wrapper.
    ///
    /// [`DbBackend::begin`] wraps this, and [`DbBackend::execute_batch`] drives it directly.
    async fn begin_transaction(&self) -> Result<RdsDataTransaction, DbError> {
        let output = self
            .client
            .begin_transaction()
            .resource_arn(self.resource_arn.as_str())
            .secret_arn(self.secret_arn.as_str())
            .database(self.database.as_str())
            .send()
            .await
            .map_err(|error| sdk_error("begin a transaction", error))?;

        let transaction_id = output.transaction_id.ok_or_else(|| {
            DbError::backend("RDS Data API began a transaction without returning a transaction id")
        })?;
        tracing::debug!(%transaction_id, "began an RDS Data API transaction");

        Ok(RdsDataTransaction {
            db: self.clone(),
            transaction_id,
        })
    }
}

impl DbBackend for RdsDataDb {
    fn dialect(&self) -> DbDialect {
        self.engine.dialect()
    }

    async fn query(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        self.run(query, params, None, RowMode::Collect).await
    }

    async fn execute(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        self.run(query, params, None, RowMode::Discard).await
    }

    async fn begin(&self) -> Result<DbTransaction, DbError> {
        Ok(DbTransaction::new(self.begin_transaction().await?))
    }

    /// Run the batch inside one explicit Data API transaction.
    ///
    /// Every statement goes through the row-returning path, so a `SELECT` inside a batch hands its
    /// rows back the way it does on the sqlx and D1 backends. The first failure rolls the whole
    /// thing back; a rollback that itself fails is reported instead of the statement's error,
    /// because then the caller genuinely does not know what landed.
    async fn execute_batch(
        &self,
        statements: Vec<BatchStatement>,
    ) -> Result<Vec<DbExecResult>, DbError> {
        let transaction = self.begin_transaction().await?;
        let mut results = Vec::with_capacity(statements.len());

        for statement in &statements {
            match transaction
                .run(&statement.sql, &statement.params, RowMode::Collect)
                .await
            {
                Ok(result) => results.push(result),
                Err(error) => {
                    tracing::warn!(
                        transaction_id = %transaction.transaction_id,
                        "rolling back an RDS Data API batch after a statement failed"
                    );
                    transaction.send_rollback().await?;
                    return Err(error);
                }
            }
        }

        transaction.send_commit().await?;
        Ok(results)
    }
}

/// An interactive RDS Data API transaction.
///
/// Statements run through this carry its transaction id, so the service runs them on the same
/// database connection and inside the same transaction. It ends at [`DbTransaction::commit`] or
/// [`DbTransaction::rollback`]; dropping it leaves the transaction open until the service's
/// three-minute idle timeout rolls it back.
#[derive(Debug)]
pub struct RdsDataTransaction {
    db: RdsDataDb,
    transaction_id: String,
}

impl RdsDataTransaction {
    /// The service's id for this transaction.
    ///
    /// Worth logging next to a slow query: it is what ties a statement to its transaction in
    /// `CloudTrail` and in Performance Insights.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// Run one statement inside this transaction.
    async fn run(
        &self,
        sql: &str,
        params: &[DbValue],
        rows: RowMode,
    ) -> Result<DbExecResult, DbError> {
        self.db
            .run(sql, params, Some(&self.transaction_id), rows)
            .await
    }

    /// Send `CommitTransaction`.
    ///
    /// Takes `&self` so both the trait's consuming [`commit`](DbTransactionBackend::commit) and the
    /// batch path can end a transaction the same way.
    async fn send_commit(&self) -> Result<(), DbError> {
        let output = self
            .db
            .client
            .commit_transaction()
            .resource_arn(self.db.resource_arn.as_str())
            .secret_arn(self.db.secret_arn.as_str())
            .transaction_id(self.transaction_id.as_str())
            .send()
            .await
            .map_err(|error| sdk_error("commit a transaction", error))?;
        tracing::debug!(
            transaction_id = %self.transaction_id,
            status = output.transaction_status(),
            "committed an RDS Data API transaction"
        );
        Ok(())
    }

    /// Send `RollbackTransaction`.
    async fn send_rollback(&self) -> Result<(), DbError> {
        let output = self
            .db
            .client
            .rollback_transaction()
            .resource_arn(self.db.resource_arn.as_str())
            .secret_arn(self.db.secret_arn.as_str())
            .transaction_id(self.transaction_id.as_str())
            .send()
            .await
            .map_err(|error| sdk_error("roll back a transaction", error))?;
        tracing::debug!(
            transaction_id = %self.transaction_id,
            status = output.transaction_status(),
            "rolled back an RDS Data API transaction"
        );
        Ok(())
    }
}

impl DbTransactionBackend for RdsDataTransaction {
    fn dialect(&self) -> DbDialect {
        self.db.engine.dialect()
    }

    async fn query(&mut self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        self.run(query, params, RowMode::Collect).await
    }

    async fn execute(&mut self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        self.run(query, params, RowMode::Discard).await
    }

    async fn commit(self) -> Result<(), DbError> {
        self.send_commit().await
    }

    async fn rollback(self) -> Result<(), DbError> {
        self.send_rollback().await
    }
}

/// Whether a statement's rows are wanted.
///
/// This is not a formatting preference: it decides whether the request asks for column metadata,
/// which the service only sends when it is asked and without which the records cannot be keyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowMode {
    /// Ask for column metadata and convert the records into JSON rows.
    Collect,
    /// Ask for neither, and report only how many rows the statement wrote.
    Discard,
}

/// Read a required environment variable, saying what it should hold when it is missing.
fn required_env(name: &str, expected: &str) -> Result<String, DbError> {
    std::env::var(name).map_err(|error| {
        DbError::backend_with(format!("{name} is not set; it must hold {expected}"), error)
    })
}

/// The result-set options pinned on every statement.
///
/// `DECIMAL → STRING` keeps a `NUMERIC` column exact instead of rounding it through a double, which
/// is what [`DbExecResult::rows`] documents; `LONG → LONG` keeps a `BIGINT` a JSON number rather
/// than a string. Both happen to be the service's current defaults, and both are stated anyway so a
/// change of default cannot quietly change what a handler deserializes.
fn result_set_options() -> ResultSetOptions {
    ResultSetOptions::builder()
        .decimal_return_type(DecimalReturnType::String)
        .long_return_type(LongReturnType::Long)
        .build()
}

/// The Data API parameter name for the `index`-th bound value, counting from one.
fn parameter_name(index: usize) -> String {
    format!("p{index}")
}

/// Bind one [`DbValue`] as a named `SqlParameter`.
///
/// See the [module documentation](self) for why the last four variants travel as hinted strings and
/// why a UUID is bound differently per engine.
fn sql_parameter(
    index: usize,
    value: &DbValue,
    engine: RdsEngine,
) -> Result<SqlParameter, DbError> {
    let builder = SqlParameter::builder().name(parameter_name(index));
    let builder = match value {
        DbValue::Null => builder.value(Field::IsNull(true)),
        DbValue::Boolean(value) => builder.value(Field::BooleanValue(*value)),
        DbValue::Integer(value) => builder.value(Field::LongValue(*value)),
        DbValue::Real(value) => builder.value(Field::DoubleValue(*value)),
        DbValue::Text(value) => builder.value(Field::StringValue(value.clone())),
        DbValue::Blob(value) => builder.value(Field::BlobValue(Blob::new(value.clone()))),
        DbValue::Timestamp(value) => builder
            .value(Field::StringValue(
                value.format(TIMESTAMP_FORMAT).to_string(),
            ))
            .type_hint(TypeHint::Timestamp),
        DbValue::Uuid(value) => match engine {
            RdsEngine::AuroraPostgres => builder
                .value(Field::StringValue(value.to_string()))
                .type_hint(TypeHint::Uuid),
            RdsEngine::AuroraMysql => {
                builder.value(Field::BlobValue(Blob::new(value.as_bytes().to_vec())))
            }
        },
        DbValue::Decimal(value) => builder
            .value(Field::StringValue(value.to_string()))
            .type_hint(TypeHint::Decimal),
        DbValue::Json(value) => builder
            .value(Field::StringValue(serde_json::to_string(value)?))
            .type_hint(TypeHint::Json),
    };
    Ok(builder.build())
}

/// Convert one `ExecuteStatement` response into a [`DbExecResult`].
fn exec_result(output: &ExecuteStatementOutput, rows: RowMode) -> Result<DbExecResult, DbError> {
    let rows = match rows {
        RowMode::Collect => rows_from_output(output)?,
        RowMode::Discard => Vec::new(),
    };
    Ok(DbExecResult {
        rows_read: rows.len() as u64,
        rows,
        // The service reports this as a signed count; a negative one is not a state it can be in,
        // and reading it as zero rows written beats refusing an otherwise successful statement.
        rows_written: u64::try_from(output.number_of_records_updated).unwrap_or(0),
    })
}

/// Convert `records` and `columnMetadata` into one JSON object per row.
fn rows_from_output(output: &ExecuteStatementOutput) -> Result<Vec<Value>, DbError> {
    let records = output.records();
    if records.is_empty() {
        return Ok(Vec::new());
    }

    let columns = output.column_metadata();
    if columns.is_empty() {
        return Err(DbError::backend(
            "RDS Data API returned rows without column metadata, so they cannot be keyed by column name",
        ));
    }

    let names = columns
        .iter()
        .map(column_key)
        .collect::<Result<Vec<_>, DbError>>()?;
    records
        .iter()
        .map(|record| row_to_json(record, &names))
        .collect()
}

/// The JSON key for a column: its label when the query gave it one, otherwise its name.
///
/// This is the same choice sqlx makes — `SELECT id AS user_id` is keyed `user_id` on every backend.
fn column_key(column: &ColumnMetadata) -> Result<&str, DbError> {
    column.label().or_else(|| column.name()).ok_or_else(|| {
        DbError::backend("RDS Data API returned a column with neither a label nor a name")
    })
}

/// Convert one record into a JSON object keyed by column.
fn row_to_json(record: &[Field], names: &[&str]) -> Result<Value, DbError> {
    if record.len() != names.len() {
        return Err(DbError::backend(format!(
            "RDS Data API returned a row of {} fields for {} columns",
            record.len(),
            names.len()
        )));
    }

    let mut object = Map::with_capacity(record.len());
    for (name, field) in names.iter().zip(record) {
        object.insert((*name).to_owned(), field_to_json(field)?);
    }
    Ok(Value::Object(object))
}

/// Convert one field into JSON, following the row conventions the other backends share.
fn field_to_json(field: &Field) -> Result<Value, DbError> {
    Ok(match field {
        Field::IsNull(_) => Value::Null,
        Field::BooleanValue(value) => Value::Bool(*value),
        Field::LongValue(value) => Value::Number(Number::from(*value)),
        // JSON has no NaN and no infinity, so they render as null — the same thing the sqlx
        // converters do with them.
        Field::DoubleValue(value) => Number::from_f64(*value).map_or(Value::Null, Value::Number),
        Field::StringValue(value) => Value::String(value.clone()),
        // JSON has no byte string, so a blob is an array of byte values, which is what a `Vec<u8>`
        // field deserializes from.
        Field::BlobValue(value) => bytes_to_json(value.as_ref()),
        Field::ArrayValue(value) => array_to_json(value)?,
        unknown => {
            return Err(DbError::backend(format!(
                "RDS Data API returned a field this SDK version does not recognize ({unknown:?}); upgrade aws-sdk-rdsdata"
            )))
        }
    })
}

/// Render bytes as the array of byte values the row contract documents.
fn bytes_to_json(bytes: &[u8]) -> Value {
    Value::Array(
        bytes
            .iter()
            .map(|byte| Value::Number(Number::from(*byte)))
            .collect(),
    )
}

/// Convert an array column — a `PostgreSQL` array, possibly nested — into a JSON array.
fn array_to_json(array: &ArrayValue) -> Result<Value, DbError> {
    let values = match array {
        ArrayValue::ArrayValues(values) => values
            .iter()
            .map(|value| value.as_ref().map_or(Ok(Value::Null), array_to_json))
            .collect::<Result<Vec<_>, DbError>>()?,
        ArrayValue::BooleanValues(values) => values
            .iter()
            .map(|value| value.map_or(Value::Null, Value::Bool))
            .collect(),
        ArrayValue::DoubleValues(values) => values
            .iter()
            .map(|value| {
                value
                    .and_then(Number::from_f64)
                    .map_or(Value::Null, Value::Number)
            })
            .collect(),
        ArrayValue::LongValues(values) => values
            .iter()
            .map(|value| value.map_or(Value::Null, |value| Value::Number(Number::from(value))))
            .collect(),
        ArrayValue::StringValues(values) => values
            .iter()
            .map(|value| value.clone().map_or(Value::Null, Value::String))
            .collect(),
        unknown => {
            return Err(DbError::backend(format!(
                "RDS Data API returned an array this SDK version does not recognize ({unknown:?}); upgrade aws-sdk-rdsdata"
            )))
        }
    };
    Ok(Value::Array(values))
}

/// Rewrite the dialect-native placeholders into the Data API's named form.
///
/// The SQL arriving here has already been rewritten once, by
/// [`Db`](skyzen_services::Db): `$1` … `$n` for Aurora `PostgreSQL`, `?` for Aurora `MySQL`. Both
/// become `:p1` … `:pN`, matching the names [`sql_parameter`] binds.
///
/// # Errors
///
/// [`DbError::SqlParse`] if the statement does not tokenize or names a parameter that was not
/// bound, and [`DbError::ParameterCountMismatch`] if the statement's placeholder count and the
/// bound values disagree.
fn rewrite_placeholders(sql: &str, engine: RdsEngine, params: usize) -> Result<String, DbError> {
    let tokens = tokenize(sql, engine)?;
    let mut rendered = String::with_capacity(sql.len() + params * 2);
    let mut mapper = LocationMapper::new(sql);
    let mut cursor = 0usize;
    let mut found = 0usize;

    for token in &tokens {
        let Some(placeholder) = bind_placeholder(token, engine) else {
            continue;
        };

        let index = placeholder_index(placeholder, found + 1, params)?;
        found += 1;

        let start = mapper.byte_index(token.span.start);
        rendered.push_str(&sql[cursor..start]);
        rendered.push(':');
        rendered.push_str(&parameter_name(index));
        // The placeholder's own text is its source text, so its length is all that is needed to
        // step over it. The token's reported span end is not: sqlparser's placeholder tokens
        // swallow a character of lookahead, and trusting the end would delete the character after
        // the placeholder.
        cursor = start + placeholder.len();
    }

    rendered.push_str(&sql[cursor..]);

    if found != params {
        return Err(DbError::ParameterCountMismatch {
            expected: found,
            actual: params,
        });
    }

    Ok(rendered)
}

/// The placeholder text of `token`, if it is a bind placeholder in this engine's dialect.
///
/// Anything inside a string literal, a dollar-quoted body, a quoted identifier or a comment is a
/// different token, so it is never mistaken for a placeholder.
fn bind_placeholder(token: &TokenWithSpan, engine: RdsEngine) -> Option<&str> {
    let Token::Placeholder(text) = &token.token else {
        return None;
    };

    match engine {
        RdsEngine::AuroraPostgres => text
            .strip_prefix('$')
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .map(|_| text.as_str()),
        RdsEngine::AuroraMysql => (text == "?").then_some(text.as_str()),
    }
}

/// Which bound value a placeholder names: the number it carries, or its position when it carries
/// none.
fn placeholder_index(placeholder: &str, position: usize, params: usize) -> Result<usize, DbError> {
    let index = match placeholder.strip_prefix('$') {
        Some(digits) => digits.parse::<usize>().map_err(|error| {
            DbError::SqlParse(format!(
                "placeholder `{placeholder}` is not numbered: {error}"
            ))
        })?,
        None => position,
    };

    if index == 0 || index > params {
        return Err(DbError::SqlParse(format!(
            "placeholder `{placeholder}` names parameter {index}, but {params} values are bound"
        )));
    }
    Ok(index)
}

/// Tokenize with the engine's own dialect, so its quoting rules apply.
///
/// The tokenizing itself — and the mapping of a tokenizer failure onto [`DbError::SqlParse`] —
/// comes from `skyzen-services`, which does the first rewriting pass over the same statement. Only
/// the engine-to-dialect choice is this crate's.
fn tokenize(sql: &str, engine: RdsEngine) -> Result<Vec<TokenWithSpan>, DbError> {
    match engine {
        RdsEngine::AuroraPostgres => tokenize_with_dialect(sql, &PostgreSqlDialect {}),
        RdsEngine::AuroraMysql => tokenize_with_dialect(sql, &MySqlDialect {}),
    }
}

/// Map an SDK error to a [`DbError`], reading its service error code first.
///
/// `action` names what was being attempted, so the message reads as a sentence in a log line.
/// [`DisplayErrorContext`] walks the whole source chain, so the service's error code appears in the
/// message instead of a bare "service error".
///
/// A rejected caller becomes [`DbError::Unauthorized`], the same unit variant `DynamoDB` and SQS
/// map their `AccessDeniedException` to. The message is dropped along with the source, which is
/// deliberate and matches the other services: "not authorized" is the whole diagnosis, and the
/// remedy is an IAM policy or a Secrets Manager secret, never the statement.
fn sdk_error<E>(action: &str, error: E) -> DbError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    match classify(&error) {
        AwsErrorCategory::Throttled => DbError::Throttled { retry_after: None },
        AwsErrorCategory::Unauthorized => {
            tracing::warn!(
                action,
                error = %DisplayErrorContext(&error),
                "the RDS Data API rejected the request as unauthorized",
            );
            DbError::Unauthorized
        }
        AwsErrorCategory::Backend => DbError::backend_with(
            format!("failed to {action}: {}", DisplayErrorContext(&error)),
            error,
        ),
    }
}

/// Classify a Data API error code, layering the service's own `ForbiddenException` on top of the
/// AWS-wide codes [`categorize`] recognizes.
///
/// Everything else stays [`AwsErrorCategory::Backend`], deliberately: `DatabaseResumingException`
/// and `DatabaseUnavailableException` are transient but are not rate limits, and reporting them as
/// throttling would tell a handler to back off for a reason that is not true.
fn classify<E: ProvideErrorMetadata>(error: &E) -> AwsErrorCategory {
    if error.code() == Some(FORBIDDEN_CODE) {
        return AwsErrorCategory::Unauthorized;
    }
    categorize(error)
}

#[cfg(test)]
mod tests {
    use super::{
        array_to_json, bind_placeholder, classify, exec_result, field_to_json, parameter_name,
        placeholder_index, rewrite_placeholders, rows_from_output, sdk_error, sql_parameter,
        ArrayValue, Blob, ColumnMetadata, DbBackend, DbDialect, DbError, DbValue,
        ExecuteStatementOutput, Field, RdsDataDb, RdsEngine, RowMode, TypeHint,
    };
    use crate::errors::AwsErrorCategory;
    use crate::errors::Coded;
    use aws_sdk_rdsdata::config::{BehaviorVersion, Credentials, Region};
    use aws_sdk_rdsdata::{Client, Config};
    use bigdecimal::BigDecimal;
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use std::str::FromStr as _;
    use uuid::Uuid;

    const RESOURCE_ARN: &str = "arn:aws:rds:us-east-1:123456789012:cluster:skyzen";
    const SECRET_ARN: &str = "arn:aws:secretsmanager:us-east-1:123456789012:secret:skyzen-db";
    const DATABASE: &str = "skyzen";

    /// A client with fixed credentials; every test here stops before any request is issued.
    fn client() -> Client {
        Client::from_conf(
            Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("us-east-1"))
                .credentials_provider(Credentials::new("AKIDTEST", "secret", None, None, "tests"))
                .build(),
        )
    }

    fn db(engine: RdsEngine) -> RdsDataDb {
        RdsDataDb::new(client(), RESOURCE_ARN, SECRET_ARN, DATABASE, engine)
    }

    fn column(label: &str) -> ColumnMetadata {
        ColumnMetadata::builder().name(label).label(label).build()
    }

    fn output(columns: Vec<ColumnMetadata>, records: Vec<Vec<Field>>) -> ExecuteStatementOutput {
        ExecuteStatementOutput::builder()
            .set_column_metadata(Some(columns))
            .set_records(Some(records))
            .build()
    }

    #[test]
    fn an_engine_parses_from_its_rds_identifier() {
        assert_eq!(
            RdsEngine::from_str("aurora-postgresql").unwrap(),
            RdsEngine::AuroraPostgres
        );
        assert_eq!(
            RdsEngine::from_str(" Aurora-MySQL ").unwrap(),
            RdsEngine::AuroraMysql
        );
    }

    #[test]
    fn an_unknown_engine_is_rejected_rather_than_guessed() {
        let error = RdsEngine::from_str("postgres").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("aurora-postgresql"), "{message}");
        assert!(message.contains("aurora-mysql"), "{message}");
    }

    #[test]
    fn the_dialect_follows_the_engine() {
        assert_eq!(db(RdsEngine::AuroraPostgres).dialect(), DbDialect::Postgres);
        assert_eq!(db(RdsEngine::AuroraMysql).dialect(), DbDialect::MySql);
    }

    #[test]
    fn postgres_placeholders_become_named_parameters() {
        assert_eq!(
            rewrite_placeholders(
                "SELECT * FROM users WHERE id = $1 AND email = $2",
                RdsEngine::AuroraPostgres,
                2
            )
            .unwrap(),
            "SELECT * FROM users WHERE id = :p1 AND email = :p2"
        );
    }

    #[test]
    fn mysql_placeholders_are_numbered_in_order() {
        assert_eq!(
            rewrite_placeholders(
                "UPDATE users SET email = ? WHERE id = ?",
                RdsEngine::AuroraMysql,
                2
            )
            .unwrap(),
            "UPDATE users SET email = :p1 WHERE id = :p2"
        );
    }

    #[test]
    fn a_placeholder_at_the_very_end_keeps_the_rest_of_the_statement() {
        assert_eq!(
            rewrite_placeholders("SELECT ?", RdsEngine::AuroraMysql, 1).unwrap(),
            "SELECT :p1"
        );
        assert_eq!(
            rewrite_placeholders("SELECT $1, 'tail'", RdsEngine::AuroraPostgres, 1).unwrap(),
            "SELECT :p1, 'tail'"
        );
    }

    #[test]
    fn postgres_literals_and_comments_survive_the_rewrite() {
        assert_eq!(
            rewrite_placeholders(
                include_str!("rds_data/postgres_literals.sql"),
                RdsEngine::AuroraPostgres,
                2
            )
            .unwrap(),
            include_str!("rds_data/postgres_literals_named.sql")
        );
    }

    #[test]
    fn mysql_literals_and_comments_survive_the_rewrite() {
        assert_eq!(
            rewrite_placeholders(
                include_str!("rds_data/mysql_literals.sql"),
                RdsEngine::AuroraMysql,
                2
            )
            .unwrap(),
            include_str!("rds_data/mysql_literals_named.sql")
        );
    }

    #[test]
    fn a_question_mark_is_not_a_placeholder_on_postgres() {
        // Skyzen rewrites every PostgreSQL bind to `$n` before this backend sees it, so a `?` that
        // reaches here belongs to the statement — a JSONB operator, say — and must be left alone.
        assert_eq!(
            rewrite_placeholders(
                "SELECT * FROM docs WHERE body ? 'key' AND id = $1",
                RdsEngine::AuroraPostgres,
                1
            )
            .unwrap(),
            "SELECT * FROM docs WHERE body ? 'key' AND id = :p1"
        );
    }

    #[test]
    fn a_parameter_count_mismatch_is_reported() {
        let error = rewrite_placeholders(
            "SELECT * FROM users WHERE id = ?",
            RdsEngine::AuroraMysql,
            2,
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                DbError::ParameterCountMismatch {
                    expected: 1,
                    actual: 2
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_placeholder_beyond_the_bound_values_is_reported() {
        let error = rewrite_placeholders(
            "SELECT * FROM users WHERE id = $3",
            RdsEngine::AuroraPostgres,
            1,
        )
        .unwrap_err();
        assert!(matches!(error, DbError::SqlParse(_)), "{error:?}");
    }

    #[test]
    fn an_unterminated_literal_fails_to_tokenize() {
        let error =
            rewrite_placeholders("SELECT 'unterminated", RdsEngine::AuroraMysql, 0).unwrap_err();
        assert!(matches!(error, DbError::SqlParse(_)), "{error:?}");
    }

    #[test]
    fn a_placeholder_index_is_read_from_the_placeholder_itself() {
        assert_eq!(placeholder_index("$2", 1, 2).unwrap(), 2);
        assert_eq!(placeholder_index("?", 3, 3).unwrap(), 3);
        assert!(placeholder_index("$0", 1, 2).is_err());
    }

    #[test]
    fn only_this_engines_placeholder_syntax_binds() {
        let tokens = super::tokenize("SELECT $1, ?", RdsEngine::AuroraPostgres).unwrap();
        let bound: Vec<&str> = tokens
            .iter()
            .filter_map(|token| bind_placeholder(token, RdsEngine::AuroraPostgres))
            .collect();
        assert_eq!(bound, vec!["$1"]);
    }

    #[test]
    fn every_db_value_maps_to_a_field() {
        let engine = RdsEngine::AuroraPostgres;
        let cases: Vec<(DbValue, Field, Option<TypeHint>)> = vec![
            (DbValue::Null, Field::IsNull(true), None),
            (DbValue::Boolean(true), Field::BooleanValue(true), None),
            (DbValue::Integer(-7), Field::LongValue(-7), None),
            (DbValue::Real(1.5), Field::DoubleValue(1.5), None),
            (
                DbValue::Text("hello".to_owned()),
                Field::StringValue("hello".to_owned()),
                None,
            ),
            (
                DbValue::Blob(vec![1, 2, 3]),
                Field::BlobValue(Blob::new(vec![1, 2, 3])),
                None,
            ),
            (
                DbValue::Timestamp(
                    DateTime::<Utc>::from_str("2024-05-05T12:34:56.123456Z").unwrap(),
                ),
                Field::StringValue("2024-05-05 12:34:56.123456".to_owned()),
                Some(TypeHint::Timestamp),
            ),
            (
                DbValue::Uuid(Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0)),
                Field::StringValue("12345678-9abc-def0-1234-56789abcdef0".to_owned()),
                Some(TypeHint::Uuid),
            ),
            (
                DbValue::Decimal(BigDecimal::from_str("12.3400").unwrap()),
                Field::StringValue("12.3400".to_owned()),
                Some(TypeHint::Decimal),
            ),
            (
                DbValue::Json(json!({"kind": "email"})),
                Field::StringValue(r#"{"kind":"email"}"#.to_owned()),
                Some(TypeHint::Json),
            ),
        ];

        for (index, (value, field, hint)) in cases.into_iter().enumerate() {
            let parameter = sql_parameter(index + 1, &value, engine).unwrap();
            assert_eq!(parameter.name(), Some(parameter_name(index + 1).as_str()));
            assert_eq!(parameter.value(), Some(&field), "{value:?}");
            assert_eq!(parameter.type_hint(), hint.as_ref(), "{value:?}");
        }
    }

    #[test]
    fn a_uuid_binds_as_bytes_on_mysql() {
        let value = DbValue::Uuid(Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0));
        let parameter = sql_parameter(1, &value, RdsEngine::AuroraMysql).unwrap();
        assert_eq!(
            parameter.value(),
            Some(&Field::BlobValue(Blob::new(
                0x1234_5678_9abc_def0_1234_5678_9abc_def0_u128
                    .to_be_bytes()
                    .to_vec()
            )))
        );
        assert_eq!(parameter.type_hint(), None);
    }

    #[test]
    fn a_timestamp_without_a_fraction_still_carries_six_digits() {
        let value = DbValue::Timestamp(DateTime::<Utc>::from_str("2024-05-05T12:34:56Z").unwrap());
        let parameter = sql_parameter(1, &value, RdsEngine::AuroraPostgres).unwrap();
        assert_eq!(
            parameter.value(),
            Some(&Field::StringValue("2024-05-05 12:34:56.000000".to_owned()))
        );
    }

    #[test]
    fn a_row_is_keyed_by_column_and_typed_per_field() {
        let result = output(
            vec![
                column("id"),
                column("name"),
                column("score"),
                column("active"),
                column("avatar"),
                column("missing"),
            ],
            vec![vec![
                Field::LongValue(7),
                Field::StringValue("ada".to_owned()),
                Field::DoubleValue(1.5),
                Field::BooleanValue(true),
                Field::BlobValue(Blob::new(vec![0, 255])),
                Field::IsNull(true),
            ]],
        );

        assert_eq!(
            rows_from_output(&result).unwrap(),
            vec![json!({
                "id": 7,
                "name": "ada",
                "score": 1.5,
                "active": true,
                "avatar": [0, 255],
                "missing": null,
            })]
        );
    }

    #[test]
    fn a_label_wins_over_the_column_name() {
        let result = output(
            vec![ColumnMetadata::builder()
                .name("id")
                .label("user_id")
                .build()],
            vec![vec![Field::LongValue(1)]],
        );
        assert_eq!(
            rows_from_output(&result).unwrap(),
            vec![json!({ "user_id": 1 })]
        );
    }

    #[test]
    fn an_unlabelled_column_falls_back_to_its_name() {
        let result = output(
            vec![ColumnMetadata::builder().name("id").build()],
            vec![vec![Field::LongValue(1)]],
        );
        assert_eq!(rows_from_output(&result).unwrap(), vec![json!({ "id": 1 })]);
    }

    #[test]
    fn a_column_with_no_identity_at_all_is_an_error() {
        let result = output(
            vec![ColumnMetadata::builder().build()],
            vec![vec![Field::LongValue(1)]],
        );
        assert!(rows_from_output(&result).is_err());
    }

    #[test]
    fn rows_without_column_metadata_are_an_error() {
        let result = ExecuteStatementOutput::builder()
            .set_records(Some(vec![vec![Field::LongValue(1)]]))
            .build();
        assert!(rows_from_output(&result).is_err());
    }

    #[test]
    fn a_row_of_the_wrong_width_is_an_error() {
        let result = output(
            vec![column("id")],
            vec![vec![Field::LongValue(1), Field::LongValue(2)]],
        );
        assert!(rows_from_output(&result).is_err());
    }

    // The `Unknown` members of the `Field` and `ArrayValue` unions are `#[non_exhaustive]`, so only
    // the SDK's own deserializer can build one and no test here can. The catch-all arms in
    // `field_to_json` and `array_to_json` still exist so that a union member added to the service
    // becomes a loud error instead of a silently dropped column.

    #[test]
    fn a_double_that_json_cannot_hold_becomes_null() {
        assert_eq!(
            field_to_json(&Field::DoubleValue(f64::NAN)).unwrap(),
            json!(null)
        );
        assert_eq!(
            field_to_json(&Field::DoubleValue(f64::INFINITY)).unwrap(),
            json!(null)
        );
    }

    #[test]
    fn arrays_convert_element_by_element_including_nested_ones() {
        let nested = ArrayValue::ArrayValues(vec![
            Some(ArrayValue::LongValues(vec![Some(1), None])),
            None,
        ]);
        assert_eq!(array_to_json(&nested).unwrap(), json!([[1, null], null]));
        assert_eq!(
            array_to_json(&ArrayValue::StringValues(vec![Some("a".to_owned()), None])).unwrap(),
            json!(["a", null])
        );
        assert_eq!(
            array_to_json(&ArrayValue::BooleanValues(vec![Some(false)])).unwrap(),
            json!([false])
        );
        assert_eq!(
            array_to_json(&ArrayValue::DoubleValues(vec![Some(2.5), None])).unwrap(),
            json!([2.5, null])
        );
    }

    #[test]
    fn a_write_reports_its_row_count_and_no_rows() {
        let result = ExecuteStatementOutput::builder()
            .number_of_records_updated(3)
            .set_records(Some(vec![vec![Field::LongValue(1)]]))
            .build();

        let discarded = exec_result(&result, RowMode::Discard).unwrap();
        assert!(discarded.rows.is_empty(), "{:?}", discarded.rows);
        assert_eq!(discarded.rows_read, 0);
        assert_eq!(discarded.rows_written, 3);
    }

    #[test]
    fn a_query_counts_the_rows_it_read() {
        let result = ExecuteStatementOutput::builder()
            .set_column_metadata(Some(vec![column("id")]))
            .set_records(Some(vec![
                vec![Field::LongValue(1)],
                vec![Field::LongValue(2)],
            ]))
            .build();

        let collected = exec_result(&result, RowMode::Collect).unwrap();
        assert_eq!(collected.rows_read, 2);
        assert_eq!(collected.rows_written, 0);
    }

    #[test]
    fn a_statement_carries_the_cluster_the_rewritten_sql_and_the_named_parameters() {
        let db = db(RdsEngine::AuroraPostgres);
        let statement = db
            .statement(
                "SELECT * FROM users WHERE id = $1",
                &[DbValue::Integer(7)],
                None,
                RowMode::Collect,
            )
            .unwrap();
        let input = statement.as_input();

        assert_eq!(input.get_resource_arn().as_deref(), Some(RESOURCE_ARN));
        assert_eq!(input.get_secret_arn().as_deref(), Some(SECRET_ARN));
        assert_eq!(input.get_database().as_deref(), Some(DATABASE));
        assert_eq!(
            input.get_sql().as_deref(),
            Some("SELECT * FROM users WHERE id = :p1")
        );
        assert_eq!(input.get_include_result_metadata(), &Some(true));
        assert_eq!(input.get_continue_after_timeout(), &Some(false));
        assert_eq!(input.get_transaction_id().as_deref(), None);

        let parameters = input.get_parameters().as_deref().unwrap();
        assert_eq!(parameters.len(), 1);
        assert_eq!(parameters[0].name(), Some("p1"));
        assert_eq!(parameters[0].value(), Some(&Field::LongValue(7)));
    }

    #[test]
    fn a_write_asks_for_no_result_metadata() {
        let db = db(RdsEngine::AuroraMysql);
        let statement = db
            .statement("DELETE FROM users", &[], None, RowMode::Discard)
            .unwrap();
        assert_eq!(
            statement.as_input().get_include_result_metadata(),
            &Some(false)
        );
    }

    #[test]
    fn every_statement_pins_how_numbers_come_back() {
        let db = db(RdsEngine::AuroraPostgres);
        let statement = db
            .statement("SELECT 1", &[], None, RowMode::Collect)
            .unwrap();
        let options = statement
            .as_input()
            .get_result_set_options()
            .clone()
            .unwrap();

        assert_eq!(
            options.decimal_return_type(),
            Some(&super::DecimalReturnType::String)
        );
        assert_eq!(
            options.long_return_type(),
            Some(&super::LongReturnType::Long)
        );
    }

    #[test]
    fn a_transactional_statement_carries_the_transaction_id() {
        let db = db(RdsEngine::AuroraPostgres);
        let statement = db
            .statement("SELECT 1", &[], Some("tx-42"), RowMode::Collect)
            .unwrap();
        assert_eq!(
            statement.as_input().get_transaction_id().as_deref(),
            Some("tx-42")
        );
    }

    #[test]
    fn ddl_can_be_asked_to_finish_after_the_call_times_out() {
        let db = db(RdsEngine::AuroraPostgres).with_continue_after_timeout(true);
        let statement = db
            .statement("CREATE INDEX ON users (email)", &[], None, RowMode::Collect)
            .unwrap();
        assert_eq!(
            statement.as_input().get_continue_after_timeout(),
            &Some(true)
        );
    }

    #[test]
    fn the_data_apis_own_permission_error_is_classified_as_unauthorized() {
        assert_eq!(
            classify(&Coded::new("ForbiddenException")),
            AwsErrorCategory::Unauthorized
        );
        assert_eq!(
            classify(&Coded::new("AccessDeniedException")),
            AwsErrorCategory::Unauthorized
        );
    }

    #[test]
    fn a_sql_error_stays_a_backend_error_with_its_source() {
        let error = sdk_error("run a statement", Coded::new("BadRequestException"));
        assert!(
            matches!(
                &error,
                DbError::Backend {
                    source: Some(_),
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("run a statement"), "{error}");
    }

    #[test]
    fn a_statement_timeout_stays_a_backend_error() {
        let error = sdk_error("run a statement", Coded::new("StatementTimeoutException"));
        assert!(matches!(error, DbError::Backend { .. }), "{error:?}");
    }

    #[test]
    fn a_resuming_cluster_is_not_reported_as_throttling() {
        let error = sdk_error("run a statement", Coded::new("DatabaseResumingException"));
        assert!(matches!(error, DbError::Backend { .. }), "{error:?}");
    }

    #[test]
    fn throttling_becomes_the_throttled_error() {
        let error = sdk_error("run a statement", Coded::new("ThrottlingException"));
        assert!(matches!(error, DbError::Throttled { .. }), "{error:?}");
    }

    #[test]
    fn an_unauthorized_call_becomes_the_unauthorized_error() {
        // The same variant `DynamoDB` and SQS map their `AccessDeniedException` to, so a handler
        // matching on "the credentials were refused" sees one shape across all three services.
        for code in ["ForbiddenException", "AccessDeniedException"] {
            let error = sdk_error("begin a transaction", Coded::new(code));
            assert!(matches!(error, DbError::Unauthorized), "{code}: {error:?}");
            assert!(error.to_string().contains("not authorized"), "{error}");
        }
    }
}
