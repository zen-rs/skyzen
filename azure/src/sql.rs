//! Azure SQL implementation of [`DbBackend`].
//!
//! Azure SQL is the one managed SQL service Skyzen could not reach before this backend: sqlx has no
//! T-SQL driver, so the [`Db`](skyzen_services::Db) API's native path covers `PostgreSQL`, `MySQL`
//! and `SQLite` and stops there. Azure Database for PostgreSQL and for MySQL are already served by
//! that path — they speak the same wire protocols as anywhere else — and only Azure SQL, which
//! speaks TDS, needed one of its own. This is it, built on [`tiberius`] behind a
//! [`deadpool`](deadpool_tiberius::deadpool) connection pool.
//!
//! # Placeholders and single-row bounds
//!
//! A caller writes `?` on every dialect. [`Db`](skyzen_services::Db) rewrites those into the
//! backend's native form before calling this crate, and for [`DbDialect::Mssql`] that form is
//! `@P1`, `@P2`, … — the names tiberius binds positionally. Nothing is rewritten a second time
//! here.
//!
//! One consequence is worth knowing: a `@P1` **written by the caller** collides with the name
//! generated for the first `?`, exactly as a hand-written `$1` collides on `PostgreSQL`. Bind with
//! `?` and let the rewriter name the parameters.
//!
//! `fetch_one` and `fetch_optional` also ask for a single-row bound, and T-SQL has no `LIMIT`, so
//! `skyzen-services` splices a `TOP (1)` in after the `SELECT` instead. Both rewrites are that
//! crate's; see [`skyzen_services::sql`] for exactly when they apply.
//!
//! # Binding and rows
//!
//! Which TDS type each [`DbValue`] is sent as, and how a column comes back as JSON, are in
//! [`values`] — including the two places T-SQL cannot represent what [`DbValue`] carries: a JSON
//! document travels as `nvarchar` because SQL Server has no JSON column type in the versions this
//! targets, and a decimal beyond 38 digits of precision is **refused** rather than rounded.
//!
//! One difference from the sqlx backends is real: **a column with no name is an error here**.
//! `SELECT COUNT(*)` produces one, and JSON has nothing to key it by; give the expression an `AS`
//! alias, which a typed row needed anyway.
//!
//! # Transactions pin a connection
//!
//! [`DbBackend::begin`] takes one connection out of the pool and keeps it for the transaction's
//! whole life. That is not an optimization: `BEGIN TRANSACTION` is *session* state, so running the
//! statements of a transaction through a pool that hands out a different connection each time
//! would spread them across sessions and commit nothing. [`AzureSqlTransaction`] therefore owns its
//! pooled connection and every statement goes through that one.
//!
//! What happens at the end matters just as much:
//!
//! - a clean `COMMIT` or `ROLLBACK` returns the connection to the pool;
//! - a `COMMIT` or `ROLLBACK` that itself **fails** closes the connection instead, because
//!   `@@TRANCOUNT` is then unknown and handing back a connection that might still be inside a
//!   transaction is the pool bug this design exists to avoid;
//! - a transaction **dropped** without either closes the connection too, and says so at `error`
//!   level. Nothing else can be done from a `Drop`, which cannot await.
//!
//! The rollback statement is `IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION` rather than a bare
//! `ROLLBACK`. SQL Server rolls a transaction back itself for batch-aborting errors — a deadlock
//! victim (`1205`) being exactly the case the error taxonomy cares about — and a bare `ROLLBACK`
//! would then fail with "no corresponding BEGIN TRANSACTION", replacing the real error with a
//! confusing one.
//!
//! [`DbBackend::execute_batch`] runs the statements inside one such transaction, rolling back on
//! the first failure and committing only once all of them have succeeded.
//!
//! # Errors
//!
//! Server error numbers are classified before they become a [`DbError`], so a handler never has to
//! match on message text: a refused login or a missing `GRANT` becomes [`DbError::Unauthorized`],
//! a resource-governance limit becomes [`DbError::Throttled`] (carrying Azure's own retry delay
//! when the message states one), and a deadlock victim becomes [`DbError::Conflict`]. See
//! [`errors`] for the numbers and for what is deliberately left a backend error.
//!
//! # Runtime
//!
//! tiberius is Tokio-based, like the rest of `skyzen-azure`: this backend must be built and used
//! from inside a Tokio runtime, which `#[skyzen::main]` and `skyzen-lambda` both provide. TLS is
//! `rustls`.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_azure::AzureSqlDb;
//! use skyzen_services::Db;
//!
//! // `AZURE_SQL_CONNECTION_STRING` holds the portal's ADO.NET connection string.
//! let db = Db::new(AzureSqlDb::from_env()?);
//! ```

pub mod connection_string;
pub mod errors;
pub mod values;

use core::fmt;

use deadpool_tiberius::{
    deadpool::managed::{Object, PoolError},
    tiberius::{error::Error as TiberiusError, Query},
    Client, Manager, Pool,
};
use skyzen_services::sql::{
    BatchStatement, DbBackend, DbDialect, DbError, DbExecResult, DbTransaction,
    DbTransactionBackend, DbValue,
};

use crate::sql::values::{row_to_json, to_param};

/// The environment variable holding the ADO.NET connection string.
const CONNECTION_STRING_ENV: &str = "AZURE_SQL_CONNECTION_STRING";

/// How many connections the pool keeps when nothing says otherwise.
///
/// Azure SQL bounds concurrent sessions per service tier — a Basic database allows 300, and the
/// smallest vCore sizes rather fewer — and every replica of an application holds its own pool, so
/// the default is deliberately modest rather than "as many as there are cores".
const DEFAULT_MAX_POOL_SIZE: usize = 10;

/// The statement that opens a transaction on the pinned connection.
const BEGIN_TRANSACTION: &str = "BEGIN TRANSACTION";

/// The statement that commits it.
const COMMIT_TRANSACTION: &str = "COMMIT TRANSACTION";

/// The statement that rolls it back, guarded because SQL Server may have rolled it back already.
///
/// See the [module documentation](self#transactions-pin-a-connection) for why the guard is not
/// optional.
const ROLLBACK_TRANSACTION: &str = "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION";

/// One connection, held for as long as its holder needs it.
type PooledClient = Object<Manager>;

/// How to reach an Azure SQL database.
///
/// Built from the ADO.NET connection string the Azure portal hands out — `Server=`, `Database=`,
/// `User ID=`, `Password=`, `Encrypt=` — which [`connection_string`] reads, including the two
/// Azure-specific policies applied to it.
#[derive(Clone)]
pub struct AzureSqlConfig {
    connection_string: String,
    max_pool_size: usize,
}

impl AzureSqlConfig {
    /// Address a database by its ADO.NET connection string.
    #[must_use]
    pub fn new(connection_string: impl Into<String>) -> Self {
        Self {
            connection_string: connection_string.into(),
            max_pool_size: DEFAULT_MAX_POOL_SIZE,
        }
    }

    /// Read the connection string from `AZURE_SQL_CONNECTION_STRING`.
    ///
    /// # Errors
    ///
    /// [`DbError::Backend`] if the variable is unset.
    pub fn from_env() -> Result<Self, DbError> {
        let connection_string = std::env::var(CONNECTION_STRING_ENV).map_err(|error| {
            DbError::backend_with(
                format!(
                    "{CONNECTION_STRING_ENV} is not set; it must hold the ADO.NET connection \
                     string for the Azure SQL database"
                ),
                error,
            )
        })?;
        Ok(Self::new(connection_string))
    }

    /// Cap how many connections the pool opens.
    ///
    /// Defaults to [`DEFAULT_MAX_POOL_SIZE`]. Every replica of an application holds its own pool,
    /// so the product of this and the replica count is what has to stay under the database's
    /// session limit.
    #[must_use]
    pub const fn with_max_pool_size(mut self, max_pool_size: usize) -> Self {
        self.max_pool_size = max_pool_size;
        self
    }
}

impl fmt::Debug for AzureSqlConfig {
    /// Renders without the connection string, which carries the password.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AzureSqlConfig")
            .field("max_pool_size", &self.max_pool_size)
            .finish_non_exhaustive()
    }
}

/// An Azure SQL database, reached over TDS through a pool of tiberius connections.
///
/// See the [module documentation](self) for the placeholder form, the type mapping, the row
/// conversion, and how transactions pin a connection.
///
/// Cloning is cheap: the pool is shared.
#[derive(Clone)]
pub struct AzureSqlDb {
    pool: Pool,
}

impl AzureSqlDb {
    /// Build a backend for `config`.
    ///
    /// Nothing is dialled: the pool opens connections lazily, so a wrong password or an unreachable
    /// server surfaces on the first query rather than here. What *is* checked here is the
    /// connection string itself — see [`connection_string::manager`].
    ///
    /// # Errors
    ///
    /// [`DbError::Backend`] when the connection string cannot be read, or asks for an
    /// authentication mechanism this backend cannot perform.
    pub fn new(config: AzureSqlConfig) -> Result<Self, DbError> {
        let AzureSqlConfig {
            connection_string,
            max_pool_size,
        } = config;
        let pool = connection_string::manager(&connection_string)?
            .max_size(max_pool_size)
            .create_pool()
            .map_err(|error| {
                DbError::backend_with(
                    format!("failed to build the Azure SQL connection pool: {error}"),
                    error,
                )
            })?;
        Ok(Self { pool })
    }

    /// Build a backend from `AZURE_SQL_CONNECTION_STRING`.
    ///
    /// # Errors
    ///
    /// [`DbError::Backend`] if the variable is unset, or if what it holds is not a connection
    /// string this backend can use.
    pub fn from_env() -> Result<Self, DbError> {
        Self::new(AzureSqlConfig::from_env()?)
    }

    /// Take a connection out of the pool.
    ///
    /// Boxed because deadpool's checkout future is around 19 KB — it holds the whole timeout and
    /// hook machinery inline — and it would otherwise be embedded in the future of every query,
    /// every transaction and every batch this backend runs.
    async fn connection(&self) -> Result<PooledClient, DbError> {
        Box::pin(self.pool.get()).await.map_err(pool_error)
    }

    /// Open a transaction on a connection of its own, returning the concrete backend.
    ///
    /// [`DbBackend::begin`] wraps this and [`DbBackend::execute_batch`] drives it directly.
    async fn begin_pinned(&self) -> Result<AzureSqlTransaction, DbError> {
        let mut connection = self.connection().await?;
        if let Err(error) =
            control_statement(&mut connection, BEGIN_TRANSACTION, "begin a transaction").await
        {
            // Whether the transaction opened is exactly what is not known here, so the connection
            // is closed rather than returned to the pool.
            drop(Object::take(connection));
            return Err(error);
        }
        Ok(AzureSqlTransaction {
            connection: Some(connection),
        })
    }
}

impl fmt::Debug for AzureSqlDb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AzureSqlDb")
            .field("status", &self.pool.status())
            .finish_non_exhaustive()
    }
}

impl DbBackend for AzureSqlDb {
    fn dialect(&self) -> DbDialect {
        DbDialect::Mssql
    }

    async fn query(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        let mut connection = self.connection().await?;
        run_query(&mut connection, query, params).await
    }

    async fn execute(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        let mut connection = self.connection().await?;
        run_execute(&mut connection, query, params).await
    }

    async fn begin(&self) -> Result<DbTransaction, DbError> {
        Ok(DbTransaction::new(self.begin_pinned().await?))
    }

    /// Run the batch inside one transaction on one connection.
    ///
    /// Every statement goes through the row-returning path, so a `SELECT` inside a batch hands its
    /// rows back the way it does on the sqlx and D1 backends. The first failure rolls the whole
    /// thing back; a rollback that itself fails is reported instead of the statement's error,
    /// because then the caller genuinely does not know what landed.
    async fn execute_batch(
        &self,
        statements: Vec<BatchStatement>,
    ) -> Result<Vec<DbExecResult>, DbError> {
        let mut transaction = self.begin_pinned().await?;
        let mut results = Vec::with_capacity(statements.len());

        for statement in &statements {
            match transaction
                .run_query(&statement.sql, &statement.params)
                .await
            {
                Ok(result) => results.push(result),
                Err(error) => {
                    tracing::warn!(
                        "rolling back an Azure SQL batch after a statement failed: {error}"
                    );
                    transaction.rollback().await?;
                    return Err(error);
                }
            }
        }

        transaction.commit().await?;
        Ok(results)
    }
}

/// An interactive Azure SQL transaction, holding the connection it runs on.
///
/// It ends at [`DbTransaction::commit`] or [`DbTransaction::rollback`]. Dropping it without either
/// closes the connection and logs an error — see the
/// [module documentation](self#transactions-pin-a-connection).
pub struct AzureSqlTransaction {
    /// `None` only after the transaction has been ended, which the consuming `commit`/`rollback`
    /// and the `Drop` impl are the only ways to do. The `Option` exists because a type with a
    /// `Drop` impl cannot have its fields moved out.
    connection: Option<PooledClient>,
}

impl AzureSqlTransaction {
    /// Borrow the pinned connection.
    fn client(&mut self) -> Result<&mut Client, DbError> {
        self.connection.as_deref_mut().ok_or_else(|| {
            DbError::backend(
                "this Azure SQL transaction has already been committed or rolled back, so it no \
                 longer holds a connection",
            )
        })
    }

    /// Run one row-returning statement inside this transaction.
    async fn run_query(&mut self, sql: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        run_query(self.client()?, sql, params).await
    }

    /// End the transaction with `sql`, returning the connection to the pool only if that worked.
    async fn finish(mut self, sql: &'static str, action: &str) -> Result<(), DbError> {
        let Some(mut connection) = self.connection.take() else {
            return Err(DbError::backend(
                "this Azure SQL transaction has already been ended",
            ));
        };

        let result = control_statement(&mut connection, sql, action).await;
        if result.is_err() {
            // `@@TRANCOUNT` is now unknown: the connection may still be inside the transaction, so
            // it is closed rather than handed to the next caller.
            drop(Object::take(connection));
        }
        result
    }
}

impl DbTransactionBackend for AzureSqlTransaction {
    fn dialect(&self) -> DbDialect {
        DbDialect::Mssql
    }

    async fn query(&mut self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        self.run_query(query, params).await
    }

    async fn execute(&mut self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        run_execute(self.client()?, query, params).await
    }

    async fn commit(self) -> Result<(), DbError> {
        self.finish(COMMIT_TRANSACTION, "commit a transaction")
            .await
    }

    async fn rollback(self) -> Result<(), DbError> {
        self.finish(ROLLBACK_TRANSACTION, "roll back a transaction")
            .await
    }
}

impl Drop for AzureSqlTransaction {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        tracing::error!(
            "an Azure SQL transaction was dropped without a commit or a rollback; its connection \
             is being closed instead of returned to the pool, because it still holds an open \
             transaction and every row that transaction touched stays locked until the server \
             notices the socket is gone. Call `commit()` or `rollback()`.",
        );
        drop(Object::take(connection));
    }
}

impl fmt::Debug for AzureSqlTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AzureSqlTransaction")
            .field("open", &self.connection.is_some())
            .finish_non_exhaustive()
    }
}

/// Build the parameterized statement both execution paths send.
fn statement(sql: &str, params: &[DbValue]) -> Result<Query<'static>, DbError> {
    let mut query = Query::new(sql.to_owned());
    for value in params {
        query.bind(to_param(value)?);
    }
    Ok(query)
}

/// Run a statement for its rows.
async fn run_query(
    client: &mut Client,
    sql: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    let stream = statement(sql, params)?
        .query(client)
        .await
        .map_err(|error| errors::db_error("run a query", error))?;
    // Only the first result set is read. One `Db::query` is one statement, so a second set can only
    // come from a caller that packed several into one string — and the portable way to do that is
    // `Db::execute_batch`, which keeps each statement's rows separate.
    let rows = stream
        .into_first_result()
        .await
        .map_err(|error| errors::db_error("read a query's rows", error))?;

    let rows = rows
        .iter()
        .map(row_to_json)
        .collect::<Result<Vec<_>, DbError>>()?;
    Ok(DbExecResult {
        rows_read: rows.len() as u64,
        rows,
        rows_written: 0,
    })
}

/// Run a statement for its row count.
async fn run_execute(
    client: &mut Client,
    sql: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    let result = statement(sql, params)?
        .execute(client)
        .await
        .map_err(|error| errors::db_error("run a statement", error))?;
    Ok(DbExecResult {
        rows: Vec::new(),
        rows_read: 0,
        rows_written: result.total(),
    })
}

/// Run one transaction-control statement as a plain SQL batch.
///
/// A batch, not a parameterized call: `BEGIN TRANSACTION` inside `sp_executesql` — which is what
/// tiberius's parameterized path uses — opens a transaction the nested batch is then expected to
/// close, and leaving it open is error `266`. Sent as a batch it is the session's transaction,
/// which is exactly what an interactive transaction has to be.
async fn control_statement(
    client: &mut Client,
    sql: &'static str,
    action: &str,
) -> Result<(), DbError> {
    let stream = client
        .simple_query(sql)
        .await
        .map_err(|error| errors::db_error(action, error))?;
    // The stream has to be consumed even though it carries no rows: an undrained one leaves the
    // connection part-way through a response, and the next statement would read this one's tokens.
    stream
        .into_results()
        .await
        .map_err(|error| errors::db_error(action, error))?;
    Ok(())
}

/// Map a pool failure onto a [`DbError`].
///
/// The interesting case is [`PoolError::Backend`]: that is the connection attempt itself failing,
/// which is where a wrong password or a server firewall rejection arrives — so it goes through the
/// same classifier a failed statement does, and becomes [`DbError::Unauthorized`] rather than an
/// opaque backend error.
fn pool_error(error: PoolError<TiberiusError>) -> DbError {
    match error {
        PoolError::Backend(error) => errors::db_error("connect to Azure SQL", error),
        other => DbError::backend_with(
            format!("failed to take a connection from the Azure SQL pool: {other}"),
            other,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AzureSqlConfig, AzureSqlDb, DbBackend, DbDialect, DEFAULT_MAX_POOL_SIZE,
        ROLLBACK_TRANSACTION,
    };
    use skyzen_services::sql::{DbError, DbValue};

    /// A connection string of the shape the portal hands out. Nothing here dials it.
    const CONNECTION_STRING: &str = "Server=tcp:skyzen.database.windows.net,1433;\
         Database=skyzen;User ID=skyzen_app;Password=s3cr3t;Encrypt=True;";

    fn db() -> AzureSqlDb {
        AzureSqlDb::new(AzureSqlConfig::new(CONNECTION_STRING)).expect("the pool should build")
    }

    #[test]
    fn the_backend_speaks_t_sql() {
        assert_eq!(db().dialect(), DbDialect::Mssql);
    }

    #[test]
    fn a_config_never_renders_its_connection_string() {
        // It carries the password, and a `Debug` that leaks one ends up in a log line.
        let rendered = format!("{:?}", AzureSqlConfig::new(CONNECTION_STRING));
        assert!(!rendered.contains("s3cr3t"), "{rendered}");
        assert!(
            !rendered.contains("skyzen.database.windows.net"),
            "{rendered}"
        );
        assert!(rendered.contains("max_pool_size"), "{rendered}");
    }

    #[test]
    fn the_pool_size_defaults_and_can_be_capped() {
        let config = AzureSqlConfig::new(CONNECTION_STRING);
        assert_eq!(config.max_pool_size, DEFAULT_MAX_POOL_SIZE);
        assert_eq!(config.with_max_pool_size(3).max_pool_size, 3);

        let db = AzureSqlDb::new(AzureSqlConfig::new(CONNECTION_STRING).with_max_pool_size(3))
            .expect("the pool should build");
        assert_eq!(db.pool.status().max_size, 3);
    }

    #[test]
    fn a_connection_string_that_cannot_be_used_fails_at_construction() {
        let error = AzureSqlDb::new(AzureSqlConfig::new(
            "Server=tcp:host;Authentication=Active Directory Default",
        ))
        .expect_err("an Entra ID mechanism is refused");
        assert!(matches!(error, DbError::Backend { .. }), "{error:?}");
    }

    #[test]
    fn the_rollback_is_guarded_so_a_server_side_rollback_does_not_become_the_reported_error() {
        // SQL Server rolls a deadlock victim's transaction back itself; a bare `ROLLBACK` would
        // then fail with "no corresponding BEGIN TRANSACTION" and hide the deadlock.
        assert!(
            ROLLBACK_TRANSACTION.contains("@@TRANCOUNT"),
            "{ROLLBACK_TRANSACTION}"
        );
    }

    #[test]
    fn a_statement_binds_one_parameter_per_value_in_order() {
        let statement = super::statement(
            "SELECT * FROM [events] WHERE id = @P1 AND label = @P2",
            &[DbValue::Integer(7), DbValue::Text("ready".to_owned())],
        )
        .expect("the parameters map");
        // `Query` keeps its parameters private, so what is assertable here is that building it
        // succeeded for every variant; the exact TDS type each value becomes is asserted in
        // `values`, where the mapping lives.
        assert!(format!("{statement:?}").contains("@P1"));
    }

    #[test]
    fn a_decimal_that_t_sql_cannot_hold_fails_before_anything_is_sent() {
        use bigdecimal::BigDecimal;
        use core::str::FromStr as _;

        let error = super::statement(
            "INSERT INTO [t] ([amount]) VALUES (@P1)",
            &[DbValue::Decimal(
                BigDecimal::from_str(&"9".repeat(39)).expect("a valid decimal"),
            )],
        )
        .expect_err("39 digits is more than `numeric` holds");
        assert!(error.to_string().contains("38 digits"), "{error}");
    }
}
